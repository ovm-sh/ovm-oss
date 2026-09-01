//! First-time setup: intro screen, config + proxy YAML generation, Codex
//! OAuth, the isolated Claude home (onboarding, infinite history, model
//! registry, generated CLAUDE.md), and the `claudex` shim.

use crate::config::{generate_api_key, ClaudexConfig};
use crate::output::{ask, say};
use crate::paths::{display, real_claude_config, real_claude_home, shim_install_dir, ClaudexDirs};
use crate::{proxy, ClaudexError, Result};
use console::style;
use serde_json::{json, Map, Value};
use std::io::Write;
use std::process::Command;

pub const ORIGIN_TWEET: &str = "https://x.com/thsottiaux/status/2076119366647894371";

/// The launch path's entry: guided setup without the final launch offer,
/// because `claudex`'s own first-run flow continues into the launch itself.
pub fn run() -> Result<()> {
    run_guided(false)
}

/// `ovm claudex setup` proper: the full guided path, ending with an offer to
/// launch right away so onboarding hands the user a running session, not a
/// list of next steps.
pub fn run_guided(offer_launch: bool) -> Result<()> {
    print_intro();
    if !confirm("Proceed?")? {
        say!("  {} Cancelled — nothing was changed.", style("✗").dim());
        return Ok(());
    }

    let dirs = ClaudexDirs::new()?;
    dirs.ensure_layout()?;

    // Config: create once, keep existing keys/registry on re-runs.
    let mut config = ClaudexConfig::load(&dirs.config_file())?.unwrap_or_default();
    if config.proxy.api_key.is_empty() {
        config.proxy.api_key = generate_api_key()?;
    }
    config.save(&dirs.config_file())?;
    write_proxy_config(&dirs, &config)?;
    say!(
        "  {} Config written → {}",
        style("✓").green(),
        display(&dirs.config_file())
    );

    ensure_claude_installed()?;

    seed_claude_home(&dirs, &config)?;
    crate::feedback::install_session_hooks(&dirs)?;
    say!(
        "  {} Isolated Claude home → {} (your ~/.claude is untouched)",
        style("✓").green(),
        display(&dirs.claude_home())
    );

    install_shims()?;

    // Proxy binary: prefer whatever already resolves (managed, pinned, or a
    // system install); otherwise download a managed copy — checksummed, from
    // upstream's GitHub releases — so setup has no brew prerequisite.
    let proxy_binary = match proxy::resolve_binary(&dirs, &config) {
        Some(binary) => {
            say!(
                "  {} cliproxyapi found ({})",
                style("✓").green(),
                binary.version_label()
            );
            binary.path().clone()
        }
        None => match crate::install::install_latest(&dirs) {
            Ok(path) => path,
            Err(error) => {
                say!(
                    "  {} Managed proxy install failed ({error}).",
                    style("!").yellow()
                );
                say!("    Retry later with `ovm claudex update`, or `brew install cliproxyapi`.");
                return Ok(());
            }
        },
    };
    offer_codex_login(&dirs, &proxy_binary, &config)?;
    offer_codex_cli()?;

    say!();
    say!(
        "  {} Done. Launch commands: {} · {} (yolo) · {} (fast).",
        style("(≈^.^≈)").green(),
        style("claudex").cyan().bold(),
        style("ccxy").cyan(),
        style("ccxf").cyan()
    );
    say!(
        "  {} there's a story behind the cats — {}",
        style("◇").magenta(),
        style("ovm story").magenta().bold()
    );
    if offer_launch && confirm("Launch claudex now?")? {
        let status = Command::new("ovm").arg("claudex").status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Step: Claude Code itself. claudex layers on an installed Claude Code, and a
/// guided setup that ends with "now go install the harness yourself" is not
/// guided. Check-then-do: already present → report and move on; absent →
/// offer `ovm install claude` (the verified-registry path) right here.
fn ensure_claude_installed() -> Result<()> {
    if let Some(version) = crate::launch::active_claude_version() {
        say!(
            "  {} Claude Code {} installed",
            style("✓").green(),
            style(&version).green()
        );
        return Ok(());
    }
    if !confirm("Claude Code isn't installed — install the latest verified release now?")? {
        say!(
            "    Skipped — claudex needs it. Run `ovm install claude latest && ovm use claude latest`, then re-run setup."
        );
        return Ok(());
    }
    // `install` deliberately never switches, and a fresh machine has nothing
    // active — so the guided path does both halves explicitly.
    let status = Command::new("ovm")
        .args(["install", "claude", "latest"])
        .status()?;
    if !status.success() {
        return Err(ClaudexError::Message(
            "Claude Code install did not complete. Run: ovm install claude latest, then re-run setup."
                .into(),
        ));
    }
    let status = Command::new("ovm")
        .args(["use", "claude", "latest"])
        .status()?;
    if !status.success() {
        return Err(ClaudexError::Message(
            "Claude Code installed but could not be activated. Run: ovm use claude latest, then re-run setup."
                .into(),
        ));
    }
    Ok(())
}

/// Step: the Codex CLI — optional, and clearly labeled as such. claudex needs
/// your ChatGPT/Codex ACCOUNT (the proxy signs in with it), never the codex
/// binary; but people arriving here often want both toolchains, so offer it
/// once. Declining, failing, or a piped stdin all leave setup successful.
fn offer_codex_cli() -> Result<()> {
    let installed = Command::new("ovm")
        .args(["current", "codex"])
        .output()
        .map(|out| out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty())
        .unwrap_or(false);
    if installed {
        say!(
            "  {} Codex CLI already installed (not required for claudex)",
            style("✓").green()
        );
        return Ok(());
    }
    if !confirm_default_no("Also install the Codex CLI? (optional — claudex doesn't need it)")? {
        return Ok(());
    }
    let status = Command::new("ovm")
        .args(["install", "codex", "latest"])
        .status()?;
    if status.success() {
        // Activate it too: an installed-but-never-switched codex leaves `cx`
        // dead on a fresh machine, which reads as a broken install.
        let _ = Command::new("ovm")
            .args(["use", "codex", "latest"])
            .status();
    } else {
        say!(
            "  {} Codex CLI install did not complete — claudex is unaffected. Retry: ovm install codex latest",
            style("!").yellow()
        );
    }
    Ok(())
}

fn print_intro() {
    say!();
    say!(
        "  {}  {}",
        style("(≈^.^≈)").magenta(),
        style("claudex — Claude Code on GPT-5.6").magenta().bold()
    );
    say!();
    // The pitch and its provenance — skipped when the caller has already made
    // them. `ovm hatch` spends chapter iii on exactly this ("one more thing —
    // have you heard of claudex?"), with the same framing and the same link,
    // twenty lines before it hands over. Saying it again in a second voice was
    // the loudest part of the seam between the two.
    if !crate::output::brief() {
        say!("  Claude Code stays your harness; GPT-5.6 Sol (via your ChatGPT/Codex");
        say!("  subscription) becomes the model, through a local CLIProxyAPI sidecar.");
        say!();
        say!("  This recipe was shared publicly by OpenAI's Codex lead:");
        say!("  {}", style(ORIGIN_TWEET).cyan().underlined());
    }
    // The risk line is never skipped: it is consent, not marketing.
    say!(
        "  {}",
        style("Unofficial integration — use at your own risk.").yellow()
    );
    say!(
        "  {}",
        style("(\"If this gets blocked, I owe you a reset.\")").yellow()
    );
    say!();
    say!("  Setup will (each step checks before it does anything):");
    say!("    1. Install Claude Code if it isn't already (verified registry)");
    // 72 columns is the budget: this line was 75 and wrapped to column 0 on a
    // narrow terminal, breaking the block. "local" was redundant beside
    // "localhost-only" anyway.
    say!("    2. Configure the CLIProxyAPI sidecar (localhost-only, random key)");
    say!("    3. Connect your Codex account via browser OAuth");
    say!("    4. Create an ISOLATED Claude home under ~/.ovm/claudex/claude —");
    say!("       your existing claude history, settings, and login stay untouched,");
    say!("       and /resume never mixes Anthropic and GPT sessions");
    say!("    5. Seed infinite history retention and the GPT-5.6 model registry");
    say!("       (/model switches between Sol, Terra, and Luna)");
    say!();
    say!("  Launch commands (y = yolo, f = fast/priority tier — stackable):");
    say!("    claudex / ccx        Sol");
    say!("    ccxy                 Sol, yolo");
    say!("    ccxf                 Sol on priority tier (main + subagents)");
    say!("    ccxyf                Sol, yolo, priority tier");
    say!();
}

fn confirm(question: &str) -> Result<bool> {
    if !console::Term::stderr().is_term() {
        return Ok(true);
    }
    ask!("  {} {} [Y/n] ", style("?").yellow().bold(), question);
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// A prompt whose safe answer is "no": optional extras must not install
/// themselves in unattended runs, so a non-terminal defaults to declining —
/// the mirror of `confirm`, whose steps are required and default to yes.
fn confirm_default_no(question: &str) -> Result<bool> {
    if !console::Term::stderr().is_term() {
        return Ok(false);
    }
    ask!("  {} {} [y/N] ", style("?").yellow().bold(), question);
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// CLIProxyAPI YAML: bind localhost only, our key, tokens inside our dir.
/// Contains the proxy key, so owner-readable only.
fn write_proxy_config(dirs: &ClaudexDirs, config: &ClaudexConfig) -> Result<()> {
    let contents = proxy_config_yaml(config, &dirs.proxy_auth_dir().to_string_lossy());
    crate::config::write_private(&dirs.proxy_config_file(), &contents)
}

/// Escape a string for safe interpolation inside a YAML double-quoted scalar.
/// The auth dir, api key, and model names all flow into the generated config;
/// a stray `"`, `\`, or newline in any of them would otherwise break out of
/// the quoted value and inject arbitrary config keys.
fn yaml_quote(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

fn proxy_config_yaml(config: &ClaudexConfig, auth_dir: &str) -> String {
    let mut yaml = format!(
        "# Generated by ovm-claudex — edits may be overwritten by `ovm claudex setup`.\n\
         host: \"127.0.0.1\"\n\
         port: {}\n\
         auth-dir: {}\n\
         api-keys:\n\
         \x20 - {}\n",
        config.proxy.port,
        yaml_quote(auth_dir),
        yaml_quote(&config.proxy.api_key)
    );

    // Fast mode: every registry model gets a `<model>-fast` alias whose
    // requests carry OpenAI's priority service tier — the same field Codex
    // CLI's fast toggle sends (`service_tier: "priority"` on the wire).
    // `claudex --fast` selects these aliases.
    let models = fast_eligible_models(config);
    yaml.push_str("\noauth-model-alias:\n  codex:\n");
    for model in &models {
        // `fork: true` ADDS the alias alongside the original name; without it
        // the alias replaces the name and base-model requests stop routing.
        yaml.push_str(&format!(
            "    - name: {}\n      alias: {}\n      fork: true\n",
            yaml_quote(model),
            yaml_quote(&format!("{model}-fast"))
        ));
    }
    yaml.push_str("\npayload:\n  override:\n    - models:\n");
    for model in &models {
        yaml.push_str(&format!(
            "        - name: {}\n          protocol: \"codex\"\n",
            yaml_quote(&format!("{model}-fast"))
        ));
    }
    yaml.push_str("      params:\n        service_tier: priority\n");
    yaml
}

/// The distinct registry models that get a fast alias.
fn fast_eligible_models(config: &ClaudexConfig) -> Vec<String> {
    let mut models = vec![
        config.models.opus.clone(),
        config.models.sonnet.clone(),
        config.models.haiku.clone(),
        config.models.default.clone(),
        config.models.subagent.clone(),
    ];
    models.extend(config.models.extra.iter().cloned());
    models.sort();
    models.dedup();
    models
}

/// Seed the isolated Claude home so first launch lands in a prompt, not in
/// onboarding or an Anthropic login screen.
fn seed_claude_home(dirs: &ClaudexDirs, config: &ClaudexConfig) -> Result<()> {
    let home = dirs.claude_home();
    std::fs::create_dir_all(&home)?;

    // .claude.json — onboarding done, imports pre-approved, theme carried over.
    let state_path = home.join(".claude.json");
    let mut state = read_json_object(&state_path)?;
    state.insert("hasCompletedOnboarding".into(), Value::Bool(true));
    state.insert(
        "hasClaudeMdExternalIncludesApproved".into(),
        Value::Bool(true),
    );
    if !state.contains_key("theme") {
        if let Some(theme) = real_theme() {
            state.insert("theme".into(), theme);
        }
    }
    write_json_object(&state_path, &state)?;

    // settings.json — infinite history + the static model/tuning env, so even
    // a bare `claude` pointed at this home behaves correctly.
    let settings_path = home.join("settings.json");
    let mut settings = read_json_object(&settings_path)?;
    settings.insert("cleanupPeriodDays".into(), json!(999_999));
    // Artifact publishing is off by default in a claudex home. Publishing sends
    // a page to claude.ai and hands back a shareable URL — an Anthropic service,
    // reached with an Anthropic login, from a session whose model is GPT-5.6
    // through someone else's subscription. Nobody choosing claudex is asking
    // for that, and it is the kind of default that only gets noticed after a
    // page exists. `disableArtifact` is the switch Claude Code itself reads
    // (`settings.disableArtifact === true`), and seeding it rather than forcing
    // an env var leaves it a default: edit the file and it stays edited.
    if !settings.contains_key("disableArtifact") {
        settings.insert("disableArtifact".into(), Value::Bool(true));
    }
    let env = settings
        .entry("env".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(env) = env {
        env.insert(
            "ANTHROPIC_DEFAULT_OPUS_MODEL".into(),
            json!(config.models.opus),
        );
        env.insert(
            "ANTHROPIC_DEFAULT_SONNET_MODEL".into(),
            json!(config.models.sonnet),
        );
        env.insert(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".into(),
            json!(config.models.haiku),
        );
        env.insert(
            "CLAUDE_CODE_SUBAGENT_MODEL".into(),
            json!(config.models.subagent),
        );
        env.insert(
            "CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY".into(),
            json!(config.tuning.max_tool_use_concurrency.to_string()),
        );
        env.insert(
            "ENABLE_TOOL_SEARCH".into(),
            json!(config.tuning.enable_tool_search.to_string()),
        );
        if config.tuning.always_enable_effort {
            env.insert("CLAUDE_CODE_ALWAYS_ENABLE_EFFORT".into(), json!("1"));
        }
    }
    write_json_object(&settings_path, &settings)?;

    // CLAUDE.md — the claudex-specific instruction layer. Never overwrite a
    // file the user has started tuning.
    let claude_md = home.join("CLAUDE.md");
    if !claude_md.exists() {
        let import_user_global = real_claude_home()
            .map(|real| real.join("CLAUDE.md").is_file())
            .unwrap_or(false);
        std::fs::write(&claude_md, claude_md_contents(config, import_user_global))?;
    }

    Ok(())
}

fn real_theme() -> Option<Value> {
    let path = real_claude_config()?;
    let contents = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&contents).ok()?;
    value.get("theme").cloned()
}

fn read_json_object(path: &std::path::Path) -> Result<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str(&contents)? {
            Value::Object(map) => Ok(map),
            _ => Err(ClaudexError::Message(format!(
                "{} is not a JSON object; refusing to overwrite it.",
                path.display()
            ))),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(error.into()),
    }
}

fn write_json_object(path: &std::path::Path, map: &Map<String, Value>) -> Result<()> {
    let mut contents = serde_json::to_string_pretty(&Value::Object(map.clone()))?;
    contents.push('\n');
    crate::config::write_atomic(path, &contents, None)
}

/// The generated model-registry instruction file. GPT-5.6-specific guidance
/// accumulates here over time; the user's own global CLAUDE.md is imported so
/// personal preferences apply in both worlds.
fn claude_md_contents(config: &ClaudexConfig, import_user_global: bool) -> String {
    let mut contents = format!(
        "# claudex — Claude Code on GPT-5.6\n\n\
         This session runs a GPT-5.6 model through Claude Code. Model registry:\n\n\
         | /model slot | backend model |\n\
         |---|---|\n\
         | opus | {} |\n\
         | sonnet | {} |\n\
         | haiku | {} |\n\
         | (subagents) | {} |\n\n\
         Switch with `/model opus|sonnet|haiku`",
        config.models.opus, config.models.sonnet, config.models.haiku, config.models.subagent
    );
    if config.models.extra.is_empty() {
        contents.push_str(".\n");
    } else {
        contents.push_str(", or by raw id: ");
        contents.push_str(&config.models.extra.join(", "));
        contents.push_str(".\n");
    }

    // Single-seat concurrency guardrail. This session runs on ONE shared
    // ChatGPT/Codex subscription seat, unlike Claude Code's native
    // high-concurrency backend. A wide subagent fan-out saturates the seat's
    // rate limit instantly, and failed agents that get re-spawned compound
    // into a runaway swarm (observed 2026-07-13: ~970 subagents in 7 minutes).
    contents.push_str(
        "\n## Running on a single subscription seat\n\n\
         Every request in this session — including subagents — shares ONE\n\
         ChatGPT/Codex seat with a real rate limit. Do NOT fan out wide\n\
         subagent swarms: prefer sequential work, or at most 2–3 parallel\n\
         subagents. When you hit a rate-limit / 429 / \"cooling down\" error,\n\
         BACK OFF and wait — never re-spawn the failed agent, which only\n\
         compounds the limit. Multi-agent reviews and large parallel audits\n\
         belong on Claude-native (`claude`), not claudex.\n",
    );

    if import_user_global {
        contents.push_str("\n@~/.claude/CLAUDE.md\n");
    }
    contents
}

/// Bare launch commands on PATH → the corresponding `ovm` invocation (plugin
/// dispatch keeps version resolution in OVM): `claudex`/`ccx`, plus `y`=yolo
/// and `f`=fast suffix variants — matching OVM's cc/ccy, cx/cxy alias family.
/// Every shim `ovm claudex setup` installs. Uninstall removes exactly these,
/// so the two can never drift and leave orphans.
pub(crate) const CLAUDEX_SHIMS: [&str; 5] = ["claudex", "ccx", "ccxy", "ccxf", "ccxyf"];

/// Ownership of an existing entry that would host a shim.
#[derive(Debug, PartialEq, Eq)]
enum ShimSlot {
    /// Nothing there — safe to write.
    Absent,
    /// A shim `ovm claudex setup` (or `ovm shortcuts`) wrote — safe to refresh.
    Ours,
    /// Anything else the user owns — never overwrite.
    Foreign,
}

/// Classify what lives at a shim path without following symlinks.
///
/// symlink_metadata is checked FIRST: read_to_string follows symlinks, so a
/// symlink to a shim-like target would otherwise pass the content check and a
/// dangling one would read as absent — either way write_atomic then replaces
/// the link itself. Any existing symlink is foreign; leave it untouched. An
/// existing-but-unreadable regular file (binary, permissions) is foreign too.
fn classify_shim(path: &std::path::Path) -> ShimSlot {
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return ShimSlot::Foreign;
    }
    match std::fs::read_to_string(path) {
        Ok(existing) if existing.starts_with("#!/bin/sh\nexec ovm ") => ShimSlot::Ours,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ShimSlot::Absent,
        _ => ShimSlot::Foreign,
    }
}

fn install_shims() -> Result<()> {
    let Some(bin_dir) = shim_install_dir() else {
        say!(
            "  {} No ~/.ovm/bin or ~/.local/bin — launch with `ovm claudex` instead of a shim.",
            style("!").yellow()
        );
        return Ok(());
    };
    let mut installed = Vec::new();
    for name in CLAUDEX_SHIMS {
        let target = name;
        let shim = bin_dir.join(name);
        match classify_shim(&shim) {
            ShimSlot::Absent | ShimSlot::Ours => {}
            ShimSlot::Foreign => {
                say!(
                    "  {} Skipped {name} shim: {} isn't ovm's — leaving it untouched.",
                    style("!").yellow(),
                    display(&shim)
                );
                continue;
            }
        }
        crate::config::write_atomic(
            &shim,
            &format!("#!/bin/sh\nexec ovm {target} \"$@\"\n"),
            Some(0o755),
        )?;
        installed.push(name);
    }
    say!(
        "  {} Shims installed → {} ({})",
        style("✓").green(),
        display(&bin_dir),
        installed.join(", ")
    );

    // Unreachable shims are a silent dud, so say so. Rare now that ~/.ovm/bin
    // is preferred — the OVM installer writes that one into the shell rc — but
    // the ~/.local/bin fallback has no such guarantee, and either can be taken
    // back off PATH by hand.
    if !dir_on_path(&bin_dir) {
        say!(
            "  {} {} is not on your PATH — add this to your shell rc:",
            style("!").yellow(),
            display(&bin_dir)
        );
        say!("      export PATH=\"{}:$PATH\"", rc_path_form(&bin_dir));
    }
    Ok(())
}

/// The rc-file spelling of a path: `$HOME`-relative, so the exported line
/// survives a moved home directory. The same form the OVM installer writes.
fn rc_path_form(path: &std::path::Path) -> String {
    let relative = dirs::home_dir().and_then(|home| {
        path.strip_prefix(home)
            .ok()
            .map(std::path::Path::to_path_buf)
    });
    match relative {
        Some(rest) => format!("$HOME/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Whether `dir` is one of the entries in the current `PATH`.
fn dir_on_path(dir: &std::path::Path) -> bool {
    let Some(path_env) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_env).any(|entry| entry == dir)
}

/// CLIProxyAPI exits with this code when the browser flow's localhost
/// callback port is already taken. That port is baked into the binary (it
/// must match the redirect URI registered with OpenAI — `-oauth-callback-port`
/// only moves the local listener, for SSH-tunnel setups), so the way past a
/// busy port is the device-code flow, which binds no local port at all.
const OAUTH_CALLBACK_PORT_IN_USE: i32 = 13;

/// What a live round-trip said about a stored grant.
enum GrantHealth {
    Working,
    Dead(String),
    /// No verified proxy to ask (down, or listener identity unconfirmed) —
    /// distinct from Working: the grant gets exercised on launch instead.
    Unverifiable,
}

/// Hand the terminal to CLIProxyAPI's interactive Codex OAuth flow.
fn offer_codex_login(
    dirs: &ClaudexDirs,
    binary: &std::path::Path,
    config: &ClaudexConfig,
) -> Result<()> {
    let mut reconnecting = false;
    if has_codex_auth(dirs) {
        match verify_existing_grant(dirs, config) {
            GrantHealth::Working => {
                say!(
                    "  {} Codex account connected (verified with a live completion).",
                    style("✓").green()
                );
                return Ok(());
            }
            GrantHealth::Unverifiable => {
                say!(
                    "  {} Codex OAuth grant present (proxy not running — verified on launch).",
                    style("✓").green()
                );
                return Ok(());
            }
            GrantHealth::Dead(why) => {
                say!(
                    "  {} Stored Codex grant is rejected upstream ({why}) — a fresh login is needed.",
                    style("!").yellow()
                );
                reconnecting = true;
            }
        }
    }
    let question = if reconnecting {
        "Reconnect your Codex account now (opens browser)?"
    } else {
        "Connect your Codex account now (opens browser)?"
    };
    // The only step that leaves the terminal, and the most consequential thing
    // this wizard does. Say what is about to happen before asking — a browser
    // opening unannounced is the moment people reach for ctrl-C.
    say!();
    say!(
        "  {} This opens OpenAI's sign-in page in your browser.",
        style("→").dim()
    );
    say!("    The grant is stored under ~/.ovm/claudex and used only by the");
    say!("    local sidecar. Nothing is sent anywhere else.");
    if !confirm(question)? {
        say!("    Skipped — run `ovm claudex setup` again when ready.");
        return Ok(());
    }
    let login_started = std::time::SystemTime::now();
    run_codex_login(dirs, binary)?;
    if reconnecting {
        retire_stale_grants(&dirs.proxy_auth_dir(), login_started);
    }
    Ok(())
}

/// A grant file on disk proves nothing: OpenAI invalidates the refresh-token
/// family when the same account logs in through the Codex CLI, and the file
/// looks identical afterward (2026-08-17: setup reported "already connected"
/// on a week-dead grant). Only a live completion through an identity-verified
/// proxy may answer "connected"; the key is never sent to an unverified
/// listener.
fn verify_existing_grant(dirs: &ClaudexDirs, config: &ClaudexConfig) -> GrantHealth {
    match proxy::probe(
        config.proxy.port,
        &config.proxy.api_key,
        proxy::ProbeIdentity::Pidfile(dirs),
    ) {
        proxy::ProxyProbe::Verified => {}
        _ => return GrantHealth::Unverifiable,
    }
    match proxy::probe_codex_credential(
        config.proxy.port,
        &config.proxy.api_key,
        &config.models.default,
    ) {
        proxy::CredentialProbe::Working => GrantHealth::Working,
        proxy::CredentialProbe::Rejected(why) => GrantHealth::Dead(why),
        proxy::CredentialProbe::Inconclusive(_) => GrantHealth::Unverifiable,
    }
}

/// Run the interactive browser login, falling back to the device-code flow
/// when the callback port is taken (dev servers love the same ports).
fn run_codex_login(dirs: &ClaudexDirs, binary: &std::path::Path) -> Result<()> {
    let status = Command::new(binary)
        .arg("--codex-login")
        .arg("--config")
        .arg(dirs.proxy_config_file())
        .status()?;
    if status.code() == Some(OAUTH_CALLBACK_PORT_IN_USE) {
        say!(
            "  {} The browser flow's local callback port is taken by another app — switching to the device-code flow.",
            style("!").yellow()
        );
        let status = Command::new(binary)
            .arg("--codex-device-login")
            .arg("--config")
            .arg(dirs.proxy_config_file())
            .status()?;
        if !status.success() {
            return Err(ClaudexError::Message(
                "Codex login did not complete. Re-run: ovm claudex setup".into(),
            ));
        }
        return Ok(());
    }
    if !status.success() {
        return Err(ClaudexError::Message(
            "Codex login did not complete. Re-run: ovm claudex setup".into(),
        ));
    }
    Ok(())
}

/// After a successful re-login over a dead grant, move grant files that
/// predate the login out of the auth dir (to `auth-retired/` beside it —
/// outside the dir the proxy loads). The proxy round-robins across every
/// stored credential, so a dead grant left beside the fresh one keeps
/// failing a share of requests. A file the login refreshed in place has a
/// newer mtime and is kept.
fn retire_stale_grants(auth_dir: &std::path::Path, login_started: std::time::SystemTime) {
    let Some(retired_dir) = auth_dir.parent().map(|parent| parent.join("auth-retired")) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(auth_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if modified >= login_started {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        if std::fs::create_dir_all(&retired_dir).is_ok()
            && std::fs::rename(&path, retired_dir.join(name)).is_ok()
        {
            say!(
                "    retired dead grant: {} → auth-retired/",
                name.to_string_lossy()
            );
        }
    }
}

/// Whether the proxy's auth dir already holds any credential file.
fn has_codex_auth(dirs: &ClaudexDirs) -> bool {
    std::fs::read_dir(dirs.proxy_auth_dir())
        .map(|entries| entries.flatten().any(|e| e.path().is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// After a re-login over a dead grant, only files predating the login are
    /// retired — the fresh grant the login just wrote must survive, and the
    /// proxy's `logs/` subdirectory is not a grant. (2026-08-17: a dead grant
    /// left beside the fresh one kept failing a share of requests, because
    /// the proxy round-robins across every stored credential.)
    #[test]
    fn retiring_stale_grants_keeps_the_fresh_one_and_subdirs() {
        use std::time::{Duration, SystemTime};
        let temp = tempfile::tempdir().unwrap();
        let auth_dir = temp.path().join("auth");
        std::fs::create_dir_all(auth_dir.join("logs")).unwrap();
        let old = auth_dir.join("codex-old.json");
        let fresh = auth_dir.join("codex-fresh.json");
        std::fs::write(&old, "{}").unwrap();
        std::fs::write(&fresh, "{}").unwrap();

        let login_started = SystemTime::now();
        let set_mtime = |path: &std::path::Path, time: SystemTime| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(time)
                .unwrap();
        };
        set_mtime(&old, login_started - Duration::from_secs(60));
        set_mtime(&fresh, login_started + Duration::from_secs(60));

        retire_stale_grants(&auth_dir, login_started);

        assert!(!old.exists(), "dead grant must leave the auth dir");
        assert!(fresh.exists(), "fresh grant must survive");
        assert!(auth_dir.join("logs").is_dir(), "subdirs are not grants");
        assert!(
            temp.path()
                .join("auth-retired")
                .join("codex-old.json")
                .exists(),
            "dead grant is retired beside the auth dir, not deleted"
        );
    }

    #[test]
    fn proxy_yaml_binds_localhost_only_with_our_key() {
        let mut config = ClaudexConfig::default();
        config.proxy.api_key = "deadbeef".into();
        let yaml = proxy_config_yaml(&config, "/x/auth");
        assert!(yaml.contains("host: \"127.0.0.1\""));
        assert!(yaml.contains("port: 8317"));
        assert!(yaml.contains("auth-dir: \"/x/auth\""));
        assert!(yaml.contains("- \"deadbeef\""));
    }

    #[test]
    fn proxy_yaml_defines_fast_aliases_with_priority_tier() {
        let yaml = proxy_config_yaml(&ClaudexConfig::default(), "/x/auth");
        // One forked alias per distinct registry model — fork keeps the base
        // name routable (without it the alias REPLACES the model; verified
        // live 2026-07-13: base requests failed with "unknown provider").
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert!(yaml.contains(&format!(
                "- name: \"{model}\"\n      alias: \"{model}-fast\"\n      fork: true"
            )));
        }
        // …with the priority service tier injected for the fast names.
        assert!(yaml.contains("service_tier: priority"));
        assert!(yaml.contains("- name: \"gpt-5.6-sol-fast\"\n          protocol: \"codex\""));
        // No duplicate alias for subagent/default (both map to sol/terra).
        assert_eq!(yaml.matches("alias: \"gpt-5.6-sol-fast\"").count(), 1);
    }

    #[test]
    fn proxy_yaml_escapes_hostile_interpolations() {
        // A quote or newline in the auth dir / api key must not break out of
        // the quoted scalar and inject config keys.
        let mut config = ClaudexConfig::default();
        config.proxy.api_key = "ab\"cd\nport: 9999".into();
        let yaml = proxy_config_yaml(&config, "/x/\"evil\n  host: 0.0.0.0/auth");
        // The literal injected key text must never appear unescaped.
        assert!(!yaml.contains("\nport: 9999"));
        assert!(!yaml.contains("\n  host: 0.0.0.0"));
        // Escaped forms are present instead.
        assert!(yaml.contains("ab\\\"cd\\nport: 9999"));
        assert!(yaml.contains("/x/\\\"evil\\n  host: 0.0.0.0/auth"));
    }

    #[test]
    fn yaml_quote_escapes_backslash_quote_and_controls() {
        assert_eq!(yaml_quote("plain"), "\"plain\"");
        assert_eq!(yaml_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(yaml_quote("a\\b"), "\"a\\\\b\"");
        assert_eq!(yaml_quote("a\nb\tc\rd"), "\"a\\nb\\tc\\rd\"");
    }

    #[test]
    #[cfg(unix)]
    fn written_proxy_config_is_owner_readable_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = crate::paths::ClaudexDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().expect("layout");

        write_proxy_config(&dirs, &ClaudexConfig::default()).expect("write");

        let mode = std::fs::metadata(dirs.proxy_config_file())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "proxy config holds the api key");
    }

    #[test]
    fn claude_md_documents_the_registry_and_imports_user_global() {
        let contents = claude_md_contents(&ClaudexConfig::default(), true);
        assert!(contents.contains("| opus | gpt-5.6-sol |"));
        assert!(contents.contains("| sonnet | gpt-5.6-terra |"));
        assert!(contents.contains("| haiku | gpt-5.6-luna |"));
        assert!(contents.contains("| (subagents) | gpt-5.6-terra |"));
        assert!(contents.contains("@~/.claude/CLAUDE.md"));
        // Single-seat concurrency guardrail must be present.
        assert!(contents.contains("single subscription seat"));
        assert!(contents.contains("never re-spawn the failed agent"));
    }

    #[test]
    fn claude_md_skips_import_when_user_has_no_global_file() {
        let contents = claude_md_contents(&ClaudexConfig::default(), false);
        assert!(!contents.contains("@~/.claude/CLAUDE.md"));
    }

    /// Publishing an artifact posts a page to claude.ai and returns a shareable
    /// URL. In a claudex home the model is GPT-5.6 on someone else's
    /// subscription, so that default is wrong — and wrong in the direction you
    /// only discover once a page exists. It must be a DEFAULT though: a user who
    /// turns it back on must not have it flipped again by the next setup run.
    #[test]
    fn artifact_publishing_is_off_by_default_but_stays_user_owned() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().expect("layout");
        let config = ClaudexConfig::default();

        seed_claude_home(&dirs, &config).expect("first seed");
        let settings_path = dirs.claude_home().join("settings.json");
        let settings = read_json_object(&settings_path).expect("read");
        assert_eq!(
            settings.get("disableArtifact"),
            Some(&serde_json::json!(true)),
            "a fresh claudex home must not publish artifacts to claude.ai"
        );

        // The user turns it back on; setup must leave that alone.
        let mut settings = read_json_object(&settings_path).expect("read");
        settings.insert("disableArtifact".into(), serde_json::json!(false));
        write_json_object(&settings_path, &settings).expect("write");

        seed_claude_home(&dirs, &config).expect("second seed");

        assert_eq!(
            read_json_object(&settings_path)
                .expect("read")
                .get("disableArtifact"),
            Some(&serde_json::json!(false)),
            "setup re-imposed a default the user had deliberately changed"
        );
    }

    #[test]
    fn seeding_is_idempotent_and_preserves_existing_keys() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().expect("layout");
        let config = ClaudexConfig::default();

        seed_claude_home(&dirs, &config).expect("first seed");

        // User customizes; re-running setup must not clobber.
        let settings_path = dirs.claude_home().join("settings.json");
        let mut settings = read_json_object(&settings_path).expect("read");
        settings.insert("outputStyle".into(), serde_json::json!("concise"));
        write_json_object(&settings_path, &settings).expect("write");
        let claude_md = dirs.claude_home().join("CLAUDE.md");
        std::fs::write(&claude_md, "# my tuned file\n").expect("write");

        seed_claude_home(&dirs, &config).expect("second seed");

        let settings = read_json_object(&settings_path).expect("read");
        assert_eq!(
            settings.get("outputStyle"),
            Some(&serde_json::json!("concise"))
        );
        assert_eq!(
            settings.get("cleanupPeriodDays"),
            Some(&serde_json::json!(999_999))
        );
        let env = settings.get("env").and_then(Value::as_object).expect("env");
        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some(&serde_json::json!("gpt-5.6-sol"))
        );
        assert_eq!(
            std::fs::read_to_string(&claude_md).expect("read"),
            "# my tuned file\n",
            "a user-tuned CLAUDE.md must never be overwritten"
        );

        let state = read_json_object(&dirs.claude_home().join(".claude.json")).expect("read");
        assert_eq!(
            state.get("hasCompletedOnboarding"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            state.get("hasClaudeMdExternalIncludesApproved"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn read_json_object_refuses_non_object_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path: PathBuf = temp.path().join("weird.json");
        std::fs::write(&path, "[1,2,3]").expect("write");
        assert!(read_json_object(&path).is_err());
    }

    #[test]
    fn classify_shim_distinguishes_absent_ours_and_foreign() {
        let temp = tempfile::tempdir().expect("tempdir");

        assert_eq!(classify_shim(&temp.path().join("nope")), ShimSlot::Absent);

        let ours = temp.path().join("ccx");
        std::fs::write(&ours, "#!/bin/sh\nexec ovm ccx \"$@\"\n").unwrap();
        assert_eq!(classify_shim(&ours), ShimSlot::Ours);

        let foreign = temp.path().join("claudex");
        std::fs::write(&foreign, "#!/bin/sh\necho my own launcher\n").unwrap();
        assert_eq!(classify_shim(&foreign), ShimSlot::Foreign);
    }

    #[test]
    #[cfg(unix)]
    fn classify_shim_treats_symlinks_as_foreign_never_ours_or_absent() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");

        // A dangling symlink must NOT read as absent — write_atomic would then
        // replace the link (and could drop the shim at the link target).
        let dangling = temp.path().join("ccx");
        symlink(temp.path().join("does-not-exist"), &dangling).unwrap();
        assert_eq!(classify_shim(&dangling), ShimSlot::Foreign);

        // A symlink whose target is a shim-like file must NOT read as ours —
        // read_to_string follows it, but we never overwrite the link's target.
        let target = temp.path().join("real-shim");
        std::fs::write(&target, "#!/bin/sh\nexec ovm ccx \"$@\"\n").unwrap();
        let link = temp.path().join("claudex");
        symlink(&target, &link).unwrap();
        assert_eq!(classify_shim(&link), ShimSlot::Foreign);
    }
}

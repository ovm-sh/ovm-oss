//! First-time setup: intro screen, config + proxy YAML generation, Codex
//! OAuth, the isolated Claude home (onboarding, infinite history, model
//! registry, generated CLAUDE.md), and the `claudex` shim.

use crate::config::{generate_api_key, ClaudexConfig};
use crate::paths::{display, real_claude_config, real_claude_home, shim_install_dir, ClaudexDirs};
use crate::{proxy, ClaudexError, Result};
use console::style;
use serde_json::{json, Map, Value};
use std::io::Write;
use std::process::Command;

pub const ORIGIN_TWEET: &str = "https://x.com/thsottiaux/status/2076119366647894371";

pub fn run() -> Result<()> {
    print_intro();
    if !confirm("Proceed?")? {
        eprintln!("  {} Cancelled — nothing was changed.", style("✗").dim());
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
    eprintln!(
        "  {} Config written → {}",
        style("✓").green(),
        display(&dirs.config_file())
    );

    seed_claude_home(&dirs, &config)?;
    crate::feedback::install_session_start_hook(&dirs)?;
    eprintln!(
        "  {} Isolated Claude home seeded → {} (your ~/.claude is untouched)",
        style("✓").green(),
        display(&dirs.claude_home())
    );

    install_shims()?;

    // Proxy binary: prefer whatever already resolves (managed, pinned, or a
    // system install); otherwise download a managed copy — checksummed, from
    // upstream's GitHub releases — so setup has no brew prerequisite.
    let proxy_binary = match proxy::resolve_binary(&dirs, &config) {
        Some(binary) => {
            eprintln!(
                "  {} cliproxyapi found ({})",
                style("✓").green(),
                binary.version_label()
            );
            binary.path().clone()
        }
        None => match crate::install::install_latest(&dirs) {
            Ok(path) => path,
            Err(error) => {
                eprintln!(
                    "  {} Managed proxy install failed ({error}).",
                    style("!").yellow()
                );
                eprintln!(
                    "    Retry later with `ovm claudex update`, or `brew install cliproxyapi`."
                );
                return Ok(());
            }
        },
    };
    offer_codex_login(&dirs, &proxy_binary)?;

    eprintln!();
    eprintln!(
        "  {} Done. Run {} to start.",
        style("(≈^.^≈)").green(),
        style("claudex").cyan().bold()
    );
    Ok(())
}

fn print_intro() {
    eprintln!();
    eprintln!(
        "  {}  {}",
        style("(≈^.^≈)").magenta(),
        style("claudex — Claude Code on GPT-5.6").magenta().bold()
    );
    eprintln!();
    eprintln!("  Claude Code stays your harness; GPT-5.6 Sol (via your ChatGPT/Codex");
    eprintln!("  subscription) becomes the model, through a local CLIProxyAPI sidecar.");
    eprintln!();
    eprintln!("  This recipe was shared publicly by OpenAI's Codex lead:");
    eprintln!("  {}", style(ORIGIN_TWEET).cyan().underlined());
    eprintln!(
        "  {}",
        style("Unofficial integration — use at your own risk. (\"If this gets blocked, I owe you a reset.\")")
            .yellow()
    );
    eprintln!();
    eprintln!("  Setup will:");
    eprintln!("    1. Configure the CLIProxyAPI sidecar (localhost-only, random local key)");
    eprintln!("    2. Connect your Codex account via browser OAuth");
    eprintln!("    3. Create an ISOLATED Claude home under ~/.ovm/claudex/claude —");
    eprintln!("       your existing claude history, settings, and login stay untouched,");
    eprintln!("       and /resume never mixes Anthropic and GPT sessions");
    eprintln!("    4. Seed infinite history retention and the GPT-5.6 model registry");
    eprintln!("       (/model switches between Sol, Terra, and Luna)");
    eprintln!();
    eprintln!("  Launch commands (y = yolo, f = fast/priority tier — stackable):");
    eprintln!("    claudex / ccx        Sol");
    eprintln!("    ccxy                 Sol, yolo");
    eprintln!("    ccxf                 Sol on priority tier (main + subagents)");
    eprintln!("    ccxyf                Sol, yolo, priority tier");
    eprintln!();
}

fn confirm(question: &str) -> Result<bool> {
    if !console::Term::stderr().is_term() {
        return Ok(true);
    }
    eprint!("  {} {} [Y/n] ", style("?").yellow().bold(), question);
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
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

/// Ownership of an existing `~/.local/bin` entry that would host a shim.
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
        eprintln!(
            "  {} ~/.local/bin not found — launch with `ovm claudex` instead of a `claudex` shim.",
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
                eprintln!(
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
    eprintln!(
        "  {} Shims installed → {} ({})",
        style("✓").green(),
        display(&bin_dir),
        installed.join(", ")
    );

    // OVM never edits shell rc files, so if nothing else put ~/.local/bin on
    // PATH (Claude Code's installer usually has), the shims are unreachable —
    // say so instead of leaving a silent dud.
    if !dir_on_path(&bin_dir) {
        eprintln!(
            "  {} {} is not on your PATH — add this to your shell rc:",
            style("!").yellow(),
            display(&bin_dir)
        );
        eprintln!("      export PATH=\"$HOME/.local/bin:$PATH\"");
    }
    Ok(())
}

/// Whether `dir` is one of the entries in the current `PATH`.
fn dir_on_path(dir: &std::path::Path) -> bool {
    let Some(path_env) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_env).any(|entry| entry == dir)
}

/// Hand the terminal to CLIProxyAPI's interactive Codex OAuth flow.
fn offer_codex_login(dirs: &ClaudexDirs, binary: &std::path::Path) -> Result<()> {
    if has_codex_auth(dirs) {
        eprintln!("  {} Codex account already connected.", style("✓").green());
        return Ok(());
    }
    if !confirm("Connect your Codex account now (opens browser)?")? {
        eprintln!("    Skipped — run `ovm claudex setup` again when ready.");
        return Ok(());
    }
    let status = Command::new(binary)
        .arg("--codex-login")
        .arg("--config")
        .arg(dirs.proxy_config_file())
        .status()?;
    if !status.success() {
        return Err(ClaudexError::Message(
            "Codex login did not complete. Re-run: ovm claudex setup".into(),
        ));
    }
    Ok(())
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

use crate::error::Result;
use console::style;

/// The bare-`ovm` screen. Six command lines, on purpose.
///
/// This is the first thing a new user sees, and OVM does one thing: it moves
/// you between versions of a few tools. Listing all ~25 commands, 11 launch
/// shortcuts and 18 examples at equal weight made that one thing hard to find.
/// Everything omitted here is one `ovm help` away, so the ceiling is enforced
/// by a test (`bare_ovm_short_help_stays_short`) rather than by good intentions.
pub fn run_short() -> Result<()> {
    print_banner();
    println!();
    println!("Manage Claude Code, Codex, and Pi versions.\n");
    println!("Usage: ovm <command> [args]\n");

    println!("Common:");
    println!("  select              pick a version interactively");
    println!("  install / use       add a version, switch to one");
    println!("  ls / current        what's available, what's active");
    println!("  update              update to latest");
    println!("  cc, cx, pi          launch (y = yolo, f = fast — ccy, cxyf, …)");
    println!("  help                everything else");

    println!(
        "\nRun `{}` for the full guide, `{}` for one command.\n",
        style("ovm help").cyan(),
        style("ovm help <command>").cyan()
    );
    Ok(())
}

fn print_banner() {
    println!();
    let face = crate::mochi::DEFAULT;
    let copy = [
        format!(
            "{} {} {}",
            style("ovm").bold(),
            style("(open version manager)").dim(),
            style(concat!("v", env!("CARGO_PKG_VERSION"))).cyan()
        ),
        style("built by mochi and quelpaw")
            .dim()
            .italic()
            .to_string(),
        style("tiny paws for big version jumps")
            .dim()
            .italic()
            .to_string(),
    ];

    for (line, copy) in face.lines().zip(copy) {
        println!("{}  {}", crate::mochi::face_style_stdout(line), copy);
    }
}

pub fn run() -> Result<()> {
    print_banner();
    println!();
    println!("Manage Claude Code, Codex, and Pi versions\n");
    println!("Usage:");
    println!("  ovm <command> <product> [args]");
    println!("  ovm <product> [args]\n");

    println!("Interactive:");
    println!("  select       Pick a version interactively (browse, install, switch)");
    println!("\nVersion management:");
    println!("  use          Switch to an installed version");
    println!("  adopt        Import an existing app version without deleting the original");
    println!("  install      Install a version (no switch)");
    println!("  uninstall    Remove an installed version");
    println!("  update       Update to latest now (`update auto` sets the launch policy)");
    println!("\nInspect:");
    println!("  ls           List versions (--remote for available, --all for both)");
    println!("  current      Show the active version");
    println!("  which        Show the active binary path");
    println!("  info         Show release notes for a version");
    println!("  stats        Installed/archived counts + disk usage per product");
    println!("\nMaintenance:");
    println!("  clean        Remove cached raw artifacts");
    println!("  cleanup      Configure old install retention");
    println!("  archive      Archive old versions");
    println!("  self         Update, switch, list, roll back, or set OVM's channel");
    println!("  self-update  Update the ovm binary itself (alias for `ovm self update`)");
    println!("  doctor       Check an installed version for compatibility issues");
    println!("  shortcuts    Install bare launch-command shims");
    // Discovered plugins (any `ovm-<name>` binary on PATH)
    let plugins: Vec<_> = crate::plugins::discover()
        .into_iter()
        .filter(|plugin| !crate::cli::is_builtin_command(&plugin.name))
        .collect();
    if !plugins.is_empty() {
        println!("\nPlugins:");
        for plugin in &plugins {
            println!("  {:<12} {}", plugin.name, style("(external)").dim());
        }
    }

    println!("\nOther:");
    println!("  completions  Generate shell completions");
    println!("  help         Show this overview");
    // One pattern, two examples — not the eight-row cross product of two flags
    // and two products. The exhaustive table lives in `ovm help launch`.
    println!("\nLaunch shortcuts:");
    println!("  cc, cx, pi   Launch the active Claude Code / Codex / Pi");
    println!("  ccx          Launch claudex (Claude Code's interface on GPT-5.6)");
    println!("  Suffixes stack onto any of those: y = yolo, f = fast (priority tier).");
    println!("  So cxy is Codex in yolo mode, and ccxyf is claudex in yolo + fast.");
    println!("  Full table: `ovm help launch`");

    println!("\nExamples:");
    println!("  ovm select                    Pick a product, then a version (interactive)");
    println!("  ovm install claude latest     Install without switching");
    println!("  ovm ls claude --remote        List available Claude versions");
    println!("  ovm update                    Update every installed product to latest");
    println!("  ovm update auto codex notify  Ask before updating Codex on launch");
    println!("  ovm self update               Update OVM itself");
    println!("  ovm cleanup 60                Keep inactive installs for 60 days");
    println!("  ovm cxy                       Launch active Codex in yolo mode");

    println!("\nSee also:");
    println!("  ovm help launch         The full launch-shortcut table");
    println!("  ovm --help              clap-generated flag reference");
    println!("  ovm <command> --help    per-command details");
    Ok(())
}

/// Per-topic help for things that are not clap subcommands. Returns `false`
/// when `topic` is not one of ours, so the caller can fall through to clap's
/// per-command help.
pub fn run_topic(topic: &str) -> bool {
    match topic {
        // Deliberately not "shortcuts": that is a real command, and
        // `ovm help shortcuts` must keep showing the command's own help.
        "launch" | "launchers" => {
            print_launch_topic();
            true
        }
        _ => false,
    }
}

/// `ovm help launch` — the exhaustive shortcut table, kept out of `ovm help`.
fn print_launch_topic() {
    println!();
    println!("{}", style("Launch shortcuts").bold());
    println!();
    println!("Every shortcut runs the active managed version of a product.");
    println!("Read them as a prefix plus optional suffixes:");
    println!();
    println!("  Prefix   cc = Claude Code   cx = Codex   pi = Pi");
    println!("           ccx = claudex (Claude Code's interface on GPT-5.6)");
    println!("  Suffix   y = yolo (skip permission prompts)");
    println!("           f = fast (OpenAI priority service tier — Codex and claudex only)");
    println!();
    println!("Full table:");
    println!("  {:<8} Claude Code", "cc");
    println!("  {:<8} Claude Code, yolo", "ccy");
    println!("  {:<8} Codex", "cx");
    println!("  {:<8} Codex, yolo", "cxy");
    println!("  {:<8} Codex, fast", "cxf");
    println!("  {:<8} Codex, yolo + fast", "cxyf");
    println!("  {:<8} Pi (no permission system, so no yolo form)", "pi");
    println!("  {:<8} claudex", "ccx");
    println!("  {:<8} claudex, yolo", "ccxy");
    println!("  {:<8} claudex, fast", "ccxf");
    println!("  {:<8} claudex, yolo + fast", "ccxyf");
    println!();
    println!("Version selectors work on every form:");
    println!("  ovm cc latest                 Move Claude Code to the newest release, then launch");
    println!("  ovm cx rust-v0.146.0          Launch one specific version (and pin it)");
    println!("  ovm ccy --ovm-version 2.1.91  One-off version, leaving the default alone");
    println!();
    println!(
        "Run `{}` to install ccy/cxy/ccx*/claudex as bare commands in ~/.local/bin,",
        style("ovm shortcuts").cyan()
    );
    println!("so you can type `ccy` instead of `ovm ccy`.");
    println!();
}

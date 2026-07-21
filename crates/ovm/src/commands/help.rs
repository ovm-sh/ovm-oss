use crate::error::Result;
use crate::product::Product;
use console::style;

/// Short help — shown on bare `ovm` or `ovm --help`.
/// ~15 lines. Points users at `ovm help` for more.
pub fn run_short() -> Result<()> {
    print_banner();
    println!();
    println!("Manage Claude Code, Codex, and Pi versions.\n");
    println!("Usage: ovm <command> [args]\n");

    println!("Common:");
    println!("  select                          Pick a version interactively");
    println!("  use <product> <version>         Switch to an installed version");
    println!("  adopt <product> [path]          Import an existing app version");
    println!("  install <product> <version>     Install a version (use `latest` for newest)");
    println!("  ls <product>                    List versions (--remote / --all for more)");
    println!("  cc, cx, pi                      Launch the active product (alias forms)");
    println!("  ccy, cxy                        Launch Claude/Codex in yolo mode");
    println!("  cxf, cxyf                       Launch Codex on priority tier (f=fast)");
    println!("  ccx, ccxy, ccxf, ccxyf          Launch claudex — Claude Code on GPT-5.6 (y=yolo, f=fast)");
    println!("  shortcuts                       Install bare ccy/cxy/ccx*/claudex commands");
    println!();
    println!(
        "  {} = {}, {}, {} (or aliases: cc, cx, pi)",
        style("<product>").dim(),
        style("claude").bold(),
        style("codex").bold(),
        style("pi").bold(),
    );

    println!("\nRun `{}` for the full guide.", style("ovm help").cyan());
    println!(
        "Run `{}` for details on a command.\n",
        style("ovm help <command>").cyan()
    );
    Ok(())
}

fn print_banner() {
    println!();
    let face = crate::mochi::DEFAULT;
    let copy = [
        format!(
            "{} {}",
            style("ovm").bold(),
            style("(open version manager)").dim()
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
        println!("{}  {}", style(line).magenta(), copy);
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
    println!("\nQuery:");
    println!("  ls           List versions (--remote for available, --all for both)");
    println!("  current      Show the active version");
    println!("  stats        Installed/archived counts + disk usage per product");
    println!("  info         Show release notes for a version");
    println!("  which        Show the active binary path");
    println!("\nMaintenance:");
    println!("  clean        Remove cached raw artifacts");
    println!("  cleanup      Configure old install retention");
    println!("  archive      Archive old versions");
    println!("  autoupdate   Set launch auto-updates: on, off, or notify (incl. `self`)");
    println!("  self         Update, switch, list, roll back, or set OVM's channel");
    // Discovered plugins (any `ovm-<name>` binary on PATH)
    let plugins = crate::plugins::discover();
    if !plugins.is_empty() {
        println!("\nPlugins:");
        for plugin in &plugins {
            println!("  {:<12} {}", plugin.name, style("(external)").dim());
        }
    }

    println!("\nOther:");
    println!("  completions  Generate shell completions");
    println!("  help         Show this overview");
    println!("\nLaunch shortcuts:");
    for product in Product::ALL {
        println!(
            "  {:<11} Launch {} using the active managed version",
            product.canonical_name(),
            product.display_name()
        );
    }
    println!("  {:<11} Launch Claude Code in yolo mode", "ccy");
    println!("  {:<11} Launch Codex in yolo mode", "cxy");
    println!("  {:<11} Launch Codex on priority service tier", "cxf");
    println!("  {:<11} Launch Codex on priority tier + yolo", "cxyf");
    println!("  {:<11} Launch claudex (Claude Code on GPT-5.6)", "ccx");
    println!("  {:<11} Launch claudex in yolo mode", "ccxy");
    println!(
        "  {:<11} Launch claudex in fast mode (priority tier)",
        "ccxf"
    );
    println!("  {:<11} Launch claudex in yolo + fast mode", "ccxyf");

    println!("\nExamples:");
    println!("  ovm select                    Pick a product, then a version (interactive)");
    println!("  ovm select claude             Browse Claude versions interactively");
    println!("  ovm select claude 2.1.91      Switch (or prompt to install)");
    println!(
        "  ovm select ovm                Switch OVM itself (alpha channel / advanced flag only)"
    );
    println!("  ovm adopt codex               Adopt the first non-OVM Codex on PATH");
    println!("  ovm install claude latest     Install without switching");
    println!("  ovm ls claude                 List installed Claude versions");
    println!("  ovm ls claude --remote        List available Claude versions");
    println!("  ovm info claude 2.1.108       Show release notes");
    println!("  ovm autoupdate on             Update to latest releases on launch");
    println!("  ovm autoupdate codex notify   Ask before updating Codex on launch");
    println!("  ovm autoupdate self off       Stop OVM updating itself on launch");
    println!(
        "  ovm self channel alpha               Alpha self-updates + list OVM in `ovm select`"
    );
    println!("  ovm self update --channel alpha      Hot-swap to the latest alpha");
    println!("  ovm self rollback                    Return to the previous OVM");
    println!("  ovm cleanup 60                Keep inactive installs for 60 days");
    println!("  ovm stats                     Installed/archived counts + disk usage");
    println!("  ovm cc                        Launch active Claude");
    println!("  ovm cxy                       Launch active Codex in yolo mode");

    println!("\nSee also:");
    println!("  ovm --help              clap-generated flag reference");
    println!("  ovm <command> --help    per-command details");
    Ok(())
}

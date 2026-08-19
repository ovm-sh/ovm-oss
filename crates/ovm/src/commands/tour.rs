//! `ovm tour` — guided onboarding, two ways.
//!
//! The opening screen offers a fork:
//!
//! - **story** — the `ovm story` chapters with an install stop after each one:
//!   Claude after Quelpaw (whose removal is why OVM exists), Codex after Mochi,
//!   claudex after Echo. A reader with no buddy in their config gets offered
//!   the real thing: install Claude Code 2.1.96, type `/buddy`, keep the cat.
//! - **tldr** — no story. Claude, Codex, and claudex, in that order, then an
//!   optional Pi.
//!
//! Both paths converge on the same acts, and every act is fail-open: a failed
//! or declined step prints how to pick it up later and the tour moves on —
//! onboarding must never strand someone half set up with an error and no path
//! forward. Acts auto-skip when the product is already managed, so on a
//! machine that has everything the story path degrades to exactly `ovm story`.

use super::story::Story;
use crate::buddy::Buddy;
use crate::error::{OvmError, Result};
use crate::plugins;
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::style;
use std::io::{self, IsTerminal, Write};
use std::process::Command;

/// The last release whose `/buddy` still hatches. Installed ephemerally for
/// the hatch act — launched via `--ovm-version`, so the user's selected
/// version never moves.
const HATCH_VERSION: &str = "2.1.96";

pub fn run() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(OvmError::Message(
            "The tour is interactive — run it from a terminal. \
             For scripted setup use `ovm install <product> latest`."
                .into(),
        ));
    }
    match choose_path()? {
        Path::Story => story_path(),
        Path::Tldr => tldr_path(),
    }
}

enum Path {
    Story,
    Tldr,
}

fn choose_path() -> Result<Path> {
    crate::mochi::say(
        crate::mochi::HAPPY,
        &format!("{}", style("Welcome.").bold()),
    );
    eprintln!();
    eprintln!("  Either way the tour sets up Claude Code, Codex, and claudex (Pi optional).");
    eprintln!(
        "  The story is why this exists — two cats and an echo. {} skips straight to setup.",
        style("n").bold()
    );
    eprintln!();
    // The same [Y/n] shape as every other prompt in the tour — a numbered
    // menu for a two-way fork read as a different UI mid-flow.
    if confirm_default_yes("Begin with the story?")? {
        Ok(Path::Story)
    } else {
        Ok(Path::Tldr)
    }
}

fn story_path() -> Result<()> {
    let story = Story::new(false);
    story.title();
    // Chapter i is Claude's chapter, so it carries Claude's setup: get a
    // managed Claude (install, or adopt one already on this machine), then
    // the buddy — theirs off their disk, or a real hatch on 2.1.96.
    story.chapter_quelpaw();
    let claude = act_claude_story()?;
    act_beat();
    match Buddy::load() {
        // A reader who was there before 2.1.97: their cat, off their disk.
        Some(buddy) => story.chapter_your_buddy(&buddy),
        // A reader who missed the window: the window is still open on 2.1.96.
        None => {
            if act_hatch()? {
                if let Some(buddy) = Buddy::load() {
                    story.chapter_your_buddy(&buddy);
                }
            }
        }
    }
    // Chapter ii is Mochi's — Codex's chapter, Codex's setup. Asked, not
    // assumed: not everyone runs Codex, and the story must not feel like a
    // funnel with chapters attached.
    story.chapter_mochi();
    let codex = act_install_asked(Product::Codex, "Do you use Codex? Set it up?")?;
    act_beat();
    // Chapter iii is Echo's — claudex's. Its own wizard asks Proceed?, so no
    // second ask here.
    story.chapter_echo();
    let claudex = act_claudex();
    let pi = act_pi()?;
    // Commands first, then the globe closes the show — the summary must not
    // be the thing on screen after "fin."
    print_summary(claude, codex, claudex, pi, false);
    story.wait_for_fin();
    story.fin();
    Ok(())
}

fn tldr_path() -> Result<()> {
    eprintln!();
    let claude = act_install(Product::Claude);
    let codex = act_install(Product::Codex);
    let claudex = act_claudex();
    let pi = act_pi()?;
    print_summary(claude, codex, claudex, pi, true);
    Ok(())
}

/// Only the commands the user can actually run now — a launch line for a
/// product whose install failed would be onboarding into an error.
fn print_summary(claude: bool, codex: bool, claudex: bool, pi: bool, mention_story: bool) {
    // Its own page, like every scene in the tour. The tour never scrolls:
    // paged output is the aesthetic, and emulators that record it (VHS)
    // reproducibly wedge on scroll-region output from non-shell processes —
    // every stall in the recording sessions happened at a would-be scroll.
    print!("\x1b[2J\x1b[H");
    let _ = io::stdout().flush();
    eprintln!();
    eprintln!("  {} Done. Your commands:", style("✓").green());
    if claude {
        eprintln!("    {}   Claude Code (yolo)", style("ovm ccy ").bold());
    }
    if codex {
        eprintln!("    {}   Codex (yolo)", style("ovm cxy ").bold());
    }
    if claudex {
        eprintln!(
            "    {}   claudex — Claude Code on GPT-5.6 (yolo)",
            style("ovm ccxy").bold()
        );
    }
    if pi {
        eprintln!("    {}   Pi", style("ovm pi  ").bold());
    }
    eprintln!(
        "    {}   browse, install, switch versions",
        style("ovm select").bold()
    );
    if mention_story {
        eprintln!();
        eprintln!(
            "  {} there's a story behind the cats — {}",
            style("◇").dim(),
            style("ovm story").bold()
        );
    }
}

// ---- acts -------------------------------------------------------------------

/// A breath between an act's last ✓ and the next chapter's screen-clear.
/// Without it the success line is wiped the same instant it prints — the
/// reader answers a prompt, the install runs, and the confirmation is gone
/// before their eyes reach it.
fn act_beat() {
    std::thread::sleep(std::time::Duration::from_millis(2000));
}

/// Chapter i's setup step. Three states, in order of likelihood for a fresh
/// tour: nothing yet (offer the install), an unmanaged Claude already on PATH
/// (offer to adopt it — the story just told them why the version you keep
/// matters, so absorbing the one they have beats installing a second), or
/// already managed (a ✓ and the time-travel command).
fn act_claude_story() -> Result<bool> {
    let vm = VersionManager::new(Product::Claude)?;
    if let Some(current) = vm.current_version()? {
        eprintln!();
        eprintln!(
            "  {} Claude Code already managed ({current}) — and the buddy era is one",
            style("✓").green(),
        );
        eprintln!(
            "    command away, anytime: {}",
            style(format!("ovm cc --ovm-version {HATCH_VERSION}")).bold(),
        );
        return Ok(true);
    }
    if let Some(foreign) = super::adopt::foreign_binary_on_path(&vm.dirs, Product::Claude) {
        eprintln!();
        eprintln!(
            "  {} Found a Claude Code already on this machine: {}",
            style("→").dim(),
            foreign.display(),
        );
        if confirm_default_yes("Adopt it into OVM (the original stays untouched)?")? {
            match super::adopt::run(&vm, None) {
                Ok(()) => return Ok(true),
                Err(error) => {
                    eprintln!(
                        "  {} Adopt didn't finish ({error}) — installing fresh instead.",
                        style("!").yellow(),
                    );
                    return Ok(act_install(Product::Claude));
                }
            }
        }
    }
    // Asked like every other act — chapter i just made the case; the reader
    // still gets to say yes.
    eprintln!();
    if confirm_default_yes("Install Claude Code (latest)?")? {
        return Ok(act_install(Product::Claude));
    }
    eprintln!(
        "  {} Skipped. Anytime: {}",
        style("→").dim(),
        style("ovm install claude latest").bold(),
    );
    Ok(false)
}

/// An act with a doorbell: ✓-skip when managed, otherwise ask before
/// installing. The story path's non-Claude products go through this — the
/// tldr path installs without asking, that being the ask.
fn act_install_asked(product: Product, question: &str) -> Result<bool> {
    let vm = VersionManager::new(product)?;
    if vm.current_version()?.is_some() {
        return Ok(act_install(product));
    }
    eprintln!();
    if confirm_default_yes(question)? {
        return Ok(act_install(product));
    }
    eprintln!(
        "  {} Skipped. Anytime: {}",
        style("→").dim(),
        style(format!("ovm install {} latest", product.canonical_name())).bold(),
    );
    Ok(false)
}

/// Install and select the latest release of `product` — unless something is
/// already managed, in which case the tour must not move it: an existing user
/// re-running the tour keeps whatever they chose.
fn act_install(product: Product) -> bool {
    let outcome = (|| -> Result<()> {
        let vm = VersionManager::new(product)?;
        if let Some(current) = vm.current_version()? {
            eprintln!(
                "  {} {} already managed ({current}) — leaving it as is",
                style("✓").green(),
                product.display_name(),
            );
            return Ok(());
        }
        eprintln!(
            "  {} Installing {} (latest)…",
            style("→").dim(),
            product.display_name(),
        );
        let latest = vm.latest_available_version()?;
        super::launch::install_and_use_latest(&vm, &latest)?;
        eprintln!(
            "  {} {} {latest} installed and selected",
            style("✓").green(),
            product.display_name(),
        );
        Ok(())
    })();
    if let Err(error) = outcome {
        // Fail-open: a flaky network must not end onboarding.
        eprintln!(
            "  {} {} install didn't finish ({error}). Later: {}",
            style("!").yellow(),
            product.display_name(),
            style(format!("ovm install {} latest", product.canonical_name())).bold(),
        );
        return false;
    }
    true
}

/// Offer the real hatch: Claude Code 2.1.96, `/buddy`, a permanent companion
/// in the reader's own config. Returns whether a launch actually happened —
/// the caller re-reads the config to see if anything hatched.
fn act_hatch() -> Result<bool> {
    eprintln!();
    eprintln!(
        "  {} You have no buddy in your config — the window is still open.",
        style("→").dim()
    );
    eprintln!(
        "    2.1.96 still hatches. It's a one-off launch of an old release
    (~190MB download); your current Claude Code selection stays put.",
    );
    if !confirm_default_no("Hatch your own?")? {
        eprintln!(
            "  {} Skipped. Anytime: {} then type {}",
            style("→").dim(),
            style(format!("ovm cc --ovm-version {HATCH_VERSION}")).bold(),
            style("/buddy").bold(),
        );
        return Ok(false);
    }
    eprintln!();
    eprintln!(
        "  {} Inside, type {} — then {} brings you back here.",
        style("→").dim(),
        style("/buddy").bold(),
        style("/exit").bold(),
    );
    eprintln!();
    // Let the instruction land before the takeover.
    std::thread::sleep(std::time::Duration::from_millis(2500));
    // Hand the TUI a clean screen. Launched at the bottom of a full, scrolled
    // page, Claude Code's inline pre-launch screens (workspace trust) paint
    // into the scroll region and can wedge terminal emulators mid-render —
    // VHS's recorder reproducibly stalls there. A cleared screen is also
    // simply the better stage direction.
    print!("\x1b[2J\x1b[H");
    let _ = io::stdout().flush();
    // A child launch rather than in-process: the launch path ends in exec.
    // The child takes the terminal, the user hatches, and the tour resumes
    // when the session ends.
    let own_exe = std::env::current_exe()?;
    let mut cmd = Command::new(own_exe);
    cmd.args(["cc", "--ovm-version", HATCH_VERSION]);
    let status = run_shielded(&mut cmd)?;
    if !status.success() {
        eprintln!(
            "  {} That launch didn't finish cleanly — the story goes on.",
            style("!").yellow()
        );
    }
    Ok(true)
}

/// Hand the terminal to the claudex guided setup, unless it has plainly
/// already run — a repeat tour must read as a clean sheet of ✓s, not re-walk
/// the whole wizard (intro, Proceed?, OAuth re-verify) for a machine that is
/// already set up. The markers checked are what setup itself creates last;
/// deeper health (a dead OAuth grant, say) is `ovm claudex doctor`'s job and
/// setup remains the documented repair path.
///
/// `--no-launch` suppresses setup's final "Launch claudex now?" — mid-tour,
/// the Pi question and the summary still follow, and an accepted launch
/// would hand the user a session with the tour's tail queued behind it.
fn act_claudex() -> bool {
    if claudex_already_set_up() {
        eprintln!(
            "  {} claudex already set up — leaving it as is (checkup: {})",
            style("✓").green(),
            style("ovm claudex doctor").bold(),
        );
        return true;
    }
    let Some(plugin) = plugins::find_bundled("claudex") else {
        eprintln!(
            "  {} claudex plugin not found — later: {}",
            style("!").yellow(),
            style("ovm claudex setup").bold(),
        );
        return false;
    };
    // Same stage direction as the hatch: the wizard opens with a ~30-line
    // intro, and starting that at the bottom of a scrolled page both looks
    // cluttered and reproducibly wedges recording emulators mid-scroll.
    print!("\x1b[2J\x1b[H");
    let _ = io::stdout().flush();
    eprintln!();
    let mut cmd = Command::new(plugin);
    cmd.args(["setup", "--no-launch"]);
    let outcome = run_shielded(&mut cmd);
    match outcome {
        Ok(status) if status.success() => true,
        _ => {
            eprintln!(
                "  {} claudex setup didn't finish — pick it up with {}",
                style("!").yellow(),
                style("ovm claudex setup").bold(),
            );
            false
        }
    }
}

/// The two on-disk markers a completed setup leaves: its config and the
/// isolated Claude home (`~/.ovm/claudex/{config.json,claude/}`).
fn claudex_already_set_up() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let base = home.join(".ovm").join("claudex");
    base.join("config.json").is_file() && base.join("claude").is_dir()
}

/// Pi sits off the golden path: offered once, default no.
fn act_pi() -> Result<bool> {
    eprintln!();
    if confirm_default_no("Also manage Pi?")? {
        return Ok(act_install(Product::Pi));
    }
    eprintln!(
        "  {} Skipped. Anytime: {}",
        style("→").dim(),
        style("ovm install pi latest").bold(),
    );
    Ok(false)
}

/// Run a terminal-taking child with the tour shielded from ctrl-C.
///
/// The hatch launch and the claudex wizard are full-screen children the user
/// may well ctrl-C out of — that is the documented way back from the hatch.
/// Their TUIs normally swallow the keypress in raw mode, but during startup
/// and shutdown the terminal is cooked and ctrl-C signals the whole
/// foreground process group: child AND tour. So the tour ignores SIGINT for
/// exactly the child's lifetime — the child gets the default handler back
/// (an ignored disposition survives exec), takes the hit, and the tour is
/// still there to catch the exit and carry on.
fn run_shielded(cmd: &mut Command) -> io::Result<std::process::ExitStatus> {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
    let previous = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let status = cmd.status();
    unsafe { libc::signal(libc::SIGINT, previous) };
    // A full-screen child does its own job control: the Claude TUI moves the
    // pty's foreground process group to itself and exits without putting it
    // back. Left stale, the NEXT child that touches the terminal is stopped
    // cold by SIGTTIN on its first stdin read — observed as the claudex
    // wizard freezing mid-intro right after a hatch. Re-take the foreground
    // ourselves, with SIGTTOU ignored for the call (a non-foreground process
    // calling tcsetpgrp is itself stopped by SIGTTOU otherwise).
    unsafe {
        if libc::isatty(libc::STDIN_FILENO) == 1 {
            let prev_ttou = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
            libc::signal(libc::SIGTTOU, prev_ttou);
        }
    }
    status
}

/// Enter means yes: the fork's happy path (the story) should be reachable by
/// leaning on Enter, mirroring the claudex wizard's own required steps.
fn confirm_default_yes(question: &str) -> Result<bool> {
    eprint!("  {} {question} [Y/n] ", style("?").yellow().bold());
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// A prompt whose safe answer is "no": the optional acts must not run
/// themselves when someone leans on Enter through the tour.
fn confirm_default_no(question: &str) -> Result<bool> {
    eprint!("  {} {question} [y/N] ", style("?").yellow().bold());
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

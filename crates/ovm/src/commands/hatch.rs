//! `ovm hatch` — guided onboarding, two ways.
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

/// The story centres its 62-column prose; act output that sits flush-left
/// beside it reads as a different program (and on the recording the content
/// block visibly jumps between centred and left-anchored pages). Every line
/// the tour prints shares this margin so the whole run keeps one left edge.
fn margin() -> &'static str {
    static MARGIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MARGIN.get_or_init(|| {
        let width = console::Term::stderr()
            .size_checked()
            .map_or(80, |(_, cols)| cols as usize);
        let margin = " ".repeat(2.max(width.saturating_sub(62) / 2));
        // The installs the tour runs print from the shared install path,
        // which knows nothing about the tour and would otherwise hug the
        // terminal edge while the prose above it sat centred.
        crate::mochi::set_indent(margin.clone());
        margin
    })
}

/// Machine-readable act outcomes, for tests and for the recording harness.
///
/// The tour is deliberately fail-open: every act catches its own error, says
/// so on screen, and carries on — so the process exits 0 whether it installed
/// everything or nothing. That makes exit status useless as a verdict, and a
/// smoke test built on it would go green forever, including on the day every
/// install breaks.
///
/// When `OVM_HATCH_EVENTS` names a file, each act appends one JSON line here.
/// A caller can then assert that no act reported `failed`, and know WHICH act
/// broke rather than diffing terminal transcripts. This is not a mode: the
/// tour behaves identically, prints identically, and nothing on screen changes
/// — it is one extra sink.
///
/// Best-effort by design: an unwritable events path must never break
/// onboarding, so every error here is swallowed.
pub(crate) const EVENTS_ENV: &str = "OVM_HATCH_EVENTS";

pub(crate) fn event(act: &str, outcome: &str, detail: &[(&str, &str)]) {
    let Some(path) = std::env::var_os(EVENTS_ENV).filter(|value| !value.is_empty()) else {
        return;
    };
    let mut record = serde_json::Map::new();
    record.insert("act".into(), serde_json::Value::from(act));
    record.insert("outcome".into(), serde_json::Value::from(outcome));
    for (key, value) in detail {
        record.insert((*key).into(), serde_json::Value::from(*value));
    }
    let Ok(mut line) = serde_json::to_string(&serde_json::Value::Object(record)) else {
        return;
    };
    line.push('\n');
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// `eprintln!` with the tour margin.
macro_rules! say {
    () => { eprintln!() };
    ($($arg:tt)*) => { eprintln!("{}{}", margin(), format_args!($($arg)*)) };
}

/// `eprint!` with the tour margin — for prompts that read on the same line.
macro_rules! ask {
    ($($arg:tt)*) => { eprint!("{}{}", margin(), format_args!($($arg)*)) };
}

pub fn run() -> Result<()> {
    // Fixes the shared margin before anything prints.
    let _ = margin();
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
    // The brand cat, on the tour's margin rather than mochi::say's flush-left
    // layout — one left edge for the whole run.
    eprintln!();
    for (index, line) in crate::mochi::HAPPY.lines().enumerate() {
        if index == 1 {
            say!(
                "{}  {}",
                crate::mochi::face_style(line),
                style("Welcome.").bold()
            );
        } else {
            say!("{}", crate::mochi::face_style(line));
        }
    }
    eprintln!();
    say!("Either way the tour sets up Claude Code, Codex, and claudex (Pi optional).");
    say!(
        "The story is why this exists — two cats and an echo. {} skips straight to setup.",
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
    // Chapter iii says Echo lives in the statusline. Offer to make that
    // literally true before moving on — the claim and the install belong on
    // the same page in the reader's head.
    act_statusline();
    // Same file, same moment: Claude deletes chat history on a timer, and
    // most people learn that the day they go looking for something old.
    act_keep_history();
    act_beat();
    // The story is over; claudex is the aside after it.
    story.one_more_thing();
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
    // The same two offers the story path makes. Skipping the story is a
    // preference about narration, not a decision to decline a statusline and
    // to let chat history expire — leaving them out meant the fast path
    // silently set you up with less.
    act_statusline();
    act_keep_history();
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
    say!("{} Done. Your commands:", style("✓").green());
    if claude {
        say!("  {}   Claude Code (yolo)", style("ovm ccy ").bold());
    }
    if codex {
        say!("  {}   Codex (yolo)", style("ovm cxy ").bold());
    }
    if claudex {
        say!(
            "  {}   claudex — Claude Code on GPT-5.6 (yolo)",
            style("ovm ccxy").bold()
        );
    }
    if pi {
        say!("  {}   Pi", style("ovm pi  ").bold());
    }
    say!(
        "  {}   browse, install, switch versions",
        style("ovm select").bold()
    );
    // The aliases are explained where they are listed, rather than three
    // chapters earlier where there was nothing yet to try them on.
    if claude || codex {
        eprintln!();
        say!(
            "{} learn about ccy and cxy at {}",
            style("◇").dim(),
            style("mochiexists.com/yolo").bold()
        );
    }
    if mention_story {
        eprintln!();
        say!(
            "{} there's a story behind the cats — the full {} tells it",
            style("◇").dim(),
            style("ovm hatch").bold()
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
        say!(
            "{} Claude Code already managed ({current}) — and the buddy era is one",
            style("✓").green(),
        );
        say!(
            "  command away, anytime: {}",
            style(format!("ovm cc --ovm-version {HATCH_VERSION}")).bold(),
        );
        return Ok(true);
    }
    if adopt_existing(&vm, Product::Claude) {
        return Ok(true);
    }
    // Asked like every other act — chapter i just made the case; the reader
    // still gets to say yes.
    eprintln!();
    if confirm_default_yes("Install Claude Code (latest)?")? {
        return Ok(act_install(Product::Claude));
    }
    say!(
        "{} Skipped. Anytime: {}",
        style("→").dim(),
        style("ovm install claude latest").bold(),
    );
    event("install", "skipped", &[("product", "claude")]);
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
    say!(
        "{} Skipped. Anytime: {}",
        style("→").dim(),
        style(format!("ovm install {} latest", product.canonical_name())).bold(),
    );
    event(
        "install",
        "skipped",
        &[("product", product.canonical_name())],
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
            say!(
                "{} {} already managed ({current}) — leaving it as is",
                style("✓").green(),
                product.display_name(),
            );
            event(
                "install",
                "ok",
                &[("product", product.canonical_name()), ("version", &current)],
            );
            return Ok(());
        }
        if adopt_existing(&vm, product) {
            return Ok(());
        }
        say!(
            "{} Installing {} (latest)…",
            style("→").dim(),
            product.display_name(),
        );
        let latest = vm.latest_available_version()?;
        super::launch::install_and_use_latest(&vm, &latest)?;
        say!(
            "{} {} {latest} installed and selected",
            style("✓").green(),
            product.display_name(),
        );
        event(
            "install",
            "ok",
            &[("product", product.canonical_name()), ("version", &latest)],
        );
        Ok(())
    })();
    if let Err(error) = outcome {
        // Fail-open: a flaky network must not end onboarding.
        say!(
            "{} {} install didn't finish ({error}). Later: {}",
            style("!").yellow(),
            product.display_name(),
            style(format!("ovm install {} latest", product.canonical_name())).bold(),
        );
        event(
            "install",
            "failed",
            &[
                ("product", product.canonical_name()),
                ("error", &error.to_string()),
            ],
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
    say!(
        "{} You have no buddy in your config — the window is still open.",
        style("→").dim()
    );
    say!("  2.1.96 still hatches. It's a one-off launch of an old release");
    say!("  (~190MB download); your current Claude Code selection stays put.");
    if !confirm_default_no("Hatch your own?")? {
        say!(
            "{} Skipped. Anytime: {} then type {}",
            style("→").dim(),
            style(format!("ovm cc --ovm-version {HATCH_VERSION}")).bold(),
            style("/buddy").bold(),
        );
        event("hatch", "skipped", &[]);
        return Ok(false);
    }
    eprintln!();
    say!(
        "{} Inside, type {} — then {} brings you back here.",
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
        say!(
            "{} That launch didn't finish cleanly — the story goes on.",
            style("!").yellow()
        );
        event(
            "hatch",
            "failed",
            &[("error", "the 2.1.96 launch exited non-zero")],
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
        say!(
            "{} claudex already set up — leaving it as is (checkup: {})",
            style("✓").green(),
            style("ovm claudex doctor").bold(),
        );
        event("claudex", "ok", &[]);
        return true;
    }
    let Some(plugin) = plugins::find_bundled("claudex") else {
        say!(
            "{} claudex plugin not found — later: {}",
            style("!").yellow(),
            style("ovm claudex setup").bold(),
        );
        event("claudex", "failed", &[("error", "plugin not bundled")]);
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
        Ok(status) if status.success() => {
            event("claudex", "ok", &[]);
            true
        }
        _ => {
            say!(
                "{} claudex setup didn't finish — pick it up with {}",
                style("!").yellow(),
                style("ovm claudex setup").bold(),
            );
            event("claudex", "failed", &[]);
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

/// Echo in the reader's own statusline — the chapter's closing claim, made
/// true. Fail-open like every other act: a refusal or a broken install must
/// not end the tour.
fn act_statusline() -> bool {
    let Ok(dirs) = crate::config::OvmDirs::new() else {
        event("statusline", "failed", &[("error", "no ovm directories")]);
        return false;
    };
    if crate::claude_settings::is_installed(&dirs.base) {
        say!("{} Echo is already in your statusline", style("✓").green());
        event("statusline", "ok", &[("already", "true")]);
        return true;
    }
    eprintln!();
    // A statusline you already set is yours: mention it, back it up, and make
    // replacing it an opt-in (default no) so leaning on Enter never clobbers a
    // companion or custom line you put there on purpose. Only a machine with no
    // statusline at all gets the default-yes offer.
    let has_own = crate::claude_settings::foreign_command(&dirs.base).map(|existing| {
        say!(
            "{} You already have a statusline: {}",
            style("!").yellow(),
            style(existing).dim(),
        );
        say!("  Kept as is unless you want Echo instead — it is backed up first either way.");
    });
    let choice = if has_own.is_some() {
        confirm_default_no("Replace it with Echo?")
    } else {
        confirm_default_yes("Put Echo in your Claude statusline?")
    };
    match choice {
        Ok(true) => {}
        _ => {
            say!(
                "{} Skipped. Anytime: {}",
                style("→").dim(),
                style("ovm statusline").bold(),
            );
            event("statusline", "skipped", &[]);
            return false;
        }
    }
    match crate::claude_settings::install(&dirs.base) {
        Ok(_) => {
            say!(
                "{} Echo is in your statusline — new Claude sessions will show them",
                style("✓").green()
            );
            event("statusline", "ok", &[]);
            true
        }
        Err(error) => {
            say!(
                "{} Statusline didn't take ({error}) — the story goes on.",
                style("!").yellow()
            );
            event("statusline", "failed", &[("error", &error.to_string())]);
            false
        }
    }
}

/// Claude Code deletes chat history after 30 days by default. The offer reads
/// their settings first so the prompt states what is actually configured
/// rather than assuming the default — someone who already changed it should
/// not be told a number that is not theirs.
fn act_keep_history() -> bool {
    use crate::claude_settings::{DEFAULT_RETENTION_DAYS, FOREVER_DAYS};
    let current = crate::claude_settings::retention_days();
    if current.is_some_and(|days| days >= FOREVER_DAYS) {
        say!(
            "{} Your Claude chat history is already kept indefinitely",
            style("✓").green()
        );
        event("retention", "ok", &[("already", "true")]);
        return true;
    }
    eprintln!();
    match current {
        Some(days) => say!(
            "{} Claude clears your chat history after {days} days.",
            style("→").dim()
        ),
        None => say!(
            "{} Claude clears your chat history after {DEFAULT_RETENTION_DAYS} days by default.",
            style("→").dim()
        ),
    }
    match confirm_default_yes("Keep your Claude chats indefinitely?") {
        Ok(true) => {}
        _ => {
            say!("{} Left as is.", style("→").dim());
            event("retention", "skipped", &[]);
            return false;
        }
    }
    match crate::claude_settings::keep_history_forever() {
        Ok(()) => {
            say!(
                "{} Chat history is kept — nothing expires now",
                style("✓").green()
            );
            event("retention", "ok", &[]);
            true
        }
        Err(error) => {
            say!(
                "{} Couldn't set retention ({error}) — the story goes on.",
                style("!").yellow()
            );
            event("retention", "failed", &[("error", &error.to_string())]);
            false
        }
    }
}

/// What the tour should do about a product it is about to set up.
///
/// Split out from the acts so it can be tested: it takes the search paths
/// rather than reading `PATH`, and a manager the caller built, so a test can
/// stand up a fake install and assert the decision without a terminal.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Setup {
    /// OVM already manages this product.
    AlreadyManaged(String),
    /// An install exists on the machine that OVM does not manage yet.
    Adopt(std::path::PathBuf),
    /// Nothing here — download it.
    InstallFresh,
}

pub(crate) fn decide_setup(
    vm: &VersionManager,
    product: Product,
    paths: &[std::path::PathBuf],
) -> Result<Setup> {
    if let Some(current) = vm.current_version()? {
        return Ok(Setup::AlreadyManaged(current));
    }
    match super::adopt::find_foreign_binary_in_paths(&vm.dirs, product, paths) {
        Some(binary) => Ok(Setup::Adopt(binary)),
        None => Ok(Setup::InstallFresh),
    }
}

/// Adopt an install already on the machine, if there is one.
///
/// Told, not asked: adopting copies the binary into OVM's store and leaves the
/// original exactly where it is, so there is no decision worth stopping a
/// reader for — only something they should see happen. Without it OVM would
/// download a second copy of what they already have, and the unmanaged one
/// would go on shadowing it on `PATH`.
fn adopt_existing(vm: &VersionManager, product: Product) -> bool {
    let paths: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    let Ok(Setup::Adopt(foreign)) = decide_setup(vm, product, &paths) else {
        return false;
    };
    say!(
        "{} Found {} already on this machine: {}",
        style("→").dim(),
        product.display_name(),
        foreign.display(),
    );
    match super::adopt::run(vm, None) {
        Ok(()) => {
            say!(
                "{} Adopted into OVM — your original is untouched",
                style("✓").green(),
            );
            event(
                "adopt",
                "ok",
                &[
                    ("product", product.canonical_name()),
                    ("from", &foreign.display().to_string()),
                ],
            );
            offer_latest_after_adopt(vm, product);
            true
        }
        Err(error) => {
            say!(
                "{} Adopt didn't finish ({error}) — installing fresh instead.",
                style("!").yellow(),
            );
            event(
                "adopt",
                "failed",
                &[
                    ("product", product.canonical_name()),
                    ("error", &error.to_string()),
                ],
            );
            false
        }
    }
}

/// Adoption pins them to whatever they already had, which for someone on a
/// year-old build means finishing the tour on a year-old build. Say the
/// version out loud and offer the newer one — installed alongside, so the
/// adopted copy stays switchable.
fn offer_latest_after_adopt(vm: &VersionManager, product: Product) {
    let Ok(Some(current)) = vm.current_version() else {
        return;
    };
    let Ok(latest) = vm.latest_available_version() else {
        return;
    };
    if latest == current {
        say!(
            "  {} that is the latest {}",
            style("✓").green(),
            product.display_name(),
        );
        return;
    }
    say!("  You are on {current}; the latest is {latest}.");
    if !matches!(
        confirm_default_yes(&format!("Install {latest} as well?")),
        Ok(true)
    ) {
        say!(
            "{} Staying on {current}. Anytime: {}",
            style("→").dim(),
            style(format!("ovm install {} latest", product.canonical_name())).bold(),
        );
        return;
    }
    match super::launch::install_and_use_latest(vm, &latest) {
        Ok(_) => say!(
            "{} {} {latest} installed and selected",
            style("✓").green(),
            product.display_name(),
        ),
        Err(error) => say!(
            "{} That didn't finish ({error}) — you are still on {current}.",
            style("!").yellow(),
        ),
    }
}

/// Pi sits off the golden path: offered once, default no.
fn act_pi() -> Result<bool> {
    eprintln!();
    // Already ovm-managed (a ✓, exactly like Claude and Codex above), or
    // present-but-unmanaged and adoptable — either way, offering to "manage Pi"
    // to someone who already has it is the wrong question.
    if let Ok(vm) = VersionManager::new(Product::Pi) {
        if vm.current_version()?.is_some() {
            return Ok(act_install(Product::Pi));
        }
        if adopt_existing(&vm, Product::Pi) {
            return Ok(true);
        }
    }
    if confirm_default_no("Also manage Pi?")? {
        return Ok(act_install(Product::Pi));
    }
    say!(
        "{} Skipped. Anytime: {}",
        style("→").dim(),
        style("ovm install pi latest").bold(),
    );
    event("pi", "skipped", &[]);
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
    ask!("{} {question} [Y/n] ", style("›").yellow().bold());
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// A prompt whose safe answer is "no": the optional acts must not run
/// themselves when someone leans on Enter through the tour.
fn confirm_default_no(question: &str) -> Result<bool> {
    ask!("{} {question} [y/N] ", style("›").yellow().bold());
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::config::{OvmConfig, OvmDirs};

    fn manager(base: &std::path::Path, product: Product) -> VersionManager {
        VersionManager::with(
            OvmDirs::at(base.join(".ovm")),
            OvmConfig::default(),
            product,
        )
    }

    fn fake_binary(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).expect("bin dir");
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\necho fake\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        path
    }

    /// The case that matters: they already have it, so OVM must not download a
    /// second copy and leave the unmanaged one shadowing it on PATH.
    #[test]
    fn an_existing_install_on_path_is_adopted_not_reinstalled() {
        let home = tempfile::tempdir().expect("tempdir");
        let bin = home.path().join("usr-local-bin");
        let binary = fake_binary(&bin, Product::Claude.binary_name());
        let vm = manager(home.path(), Product::Claude);

        let decision = decide_setup(&vm, Product::Claude, &[bin]).expect("decide");
        assert_eq!(decision, Setup::Adopt(binary));
    }

    #[test]
    fn nothing_on_path_means_a_fresh_install() {
        let home = tempfile::tempdir().expect("tempdir");
        let empty = home.path().join("empty");
        std::fs::create_dir_all(&empty).expect("dir");
        let vm = manager(home.path(), Product::Claude);

        assert_eq!(
            decide_setup(&vm, Product::Claude, &[empty]).expect("decide"),
            Setup::InstallFresh
        );
    }

    /// Codex gets the same treatment as Claude — it previously installed fresh
    /// and only warned that the unmanaged copy might shadow it.
    #[test]
    fn codex_is_adopted_the_same_way_claude_is() {
        let home = tempfile::tempdir().expect("tempdir");
        let bin = home.path().join("bin");
        let binary = fake_binary(&bin, Product::Codex.binary_name());
        let vm = manager(home.path(), Product::Codex);

        assert_eq!(
            decide_setup(&vm, Product::Codex, &[bin]).expect("decide"),
            Setup::Adopt(binary)
        );
    }

    /// And Pi, so the tour treats all three alike.
    #[test]
    fn pi_is_adopted_the_same_way_too() {
        let home = tempfile::tempdir().expect("tempdir");
        let bin = home.path().join("bin");
        let binary = fake_binary(&bin, Product::Pi.binary_name());
        let vm = manager(home.path(), Product::Pi);

        assert_eq!(
            decide_setup(&vm, Product::Pi, &[bin]).expect("decide"),
            Setup::Adopt(binary)
        );
    }

    /// A binary inside OVM's own store is not a foreign install; adopting it
    /// would be OVM adopting itself.
    #[test]
    fn ovms_own_binaries_are_not_treated_as_foreign() {
        let home = tempfile::tempdir().expect("tempdir");
        let vm = manager(home.path(), Product::Claude);
        let ovm_bin = vm.dirs.bin.clone();
        fake_binary(&ovm_bin, Product::Claude.binary_name());

        assert_eq!(
            decide_setup(&vm, Product::Claude, &[ovm_bin]).expect("decide"),
            Setup::InstallFresh
        );
    }

    /// The whole reason this sink exists: the tour swallows every error and
    /// exits 0, so a smoke test that trusts the exit code passes on the day
    /// every install breaks. The event log is what makes a failed act legible.
    #[test]
    fn a_failed_act_is_recorded_even_though_the_tour_would_exit_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("events.jsonl");
        temp_env(&log, || {
            event(
                "install",
                "ok",
                &[("product", "claude"), ("version", "2.1.0")],
            );
            event(
                "install",
                "failed",
                &[("product", "codex"), ("error", "boom")],
            );
        });

        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&log)
            .expect("log written")
            .lines()
            .map(|line| serde_json::from_str(line).expect("one json object per line"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["act"], "install");
        assert_eq!(lines[0]["version"], "2.1.0");
        let failed: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|line| line["outcome"] == "failed")
            .collect();
        assert_eq!(failed.len(), 1, "the smoke test's whole assertion");
        assert_eq!(failed[0]["product"], "codex");
        assert_eq!(failed[0]["error"], "boom");
    }

    /// Unset means silent: the tour must behave identically for real users.
    #[test]
    fn without_the_env_var_nothing_is_written() {
        let before = std::env::var_os(EVENTS_ENV);
        std::env::remove_var(EVENTS_ENV);
        event("install", "ok", &[]);
        if let Some(value) = before {
            std::env::set_var(EVENTS_ENV, value);
        }
    }

    /// An unwritable path must never break onboarding — the sink is
    /// best-effort by design.
    #[test]
    fn an_unwritable_events_path_does_not_break_the_tour() {
        let dir = tempfile::tempdir().expect("tempdir");
        let unwritable = dir.path().join("no-such-dir").join("events.jsonl");
        temp_env(&unwritable, || event("install", "ok", &[]));
    }

    /// `set_var` is process-wide; these tests serialise on it.
    fn temp_env(path: &std::path::Path, body: impl FnOnce()) {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let before = std::env::var_os(EVENTS_ENV);
        std::env::set_var(EVENTS_ENV, path);
        body();
        match before {
            Some(value) => std::env::set_var(EVENTS_ENV, value),
            None => std::env::remove_var(EVENTS_ENV),
        }
    }
}

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

use super::shortcuts;
use super::story::Story;
use crate::buddy::Buddy;
use crate::error::{OvmError, Result};
use crate::plugins;
use crate::product::Product;
use crate::version_manager::VersionManager;
use console::style;
use std::io::{self, IsTerminal, Write};
use std::process::Command;

/// The last release whose `/buddy` still hatches.
///
/// The hatch act SWITCHES to it rather than borrowing it for one launch: the
/// act exists to teach `ovm switch`, and a gesture that silently undoes itself
/// afterwards would teach the wrong thing. The reader is told how to come back
/// (`ovm cc latest`) once they have met their buddy.
const HATCH_VERSION: &str = "2.1.96";

/// The story centres its 62-column prose; act output that sits flush-left
/// beside it reads as a different program (and on the recording the content
/// block visibly jumps between centred and left-anchored pages). Every line
/// the tour prints shares this margin so the whole run keeps one left edge.
///
/// `usable` is the other half of that arrangement, and the half that used to
/// live nowhere: a centred block leaves `width - margin` columns, and the
/// terminal does not wrap what overruns them politely — the remainder starts
/// at column 0, outside the margin, and the centred block comes apart. That
/// shipped in 0.1.7 (a 91-character line on an 84-column budget) and was
/// caught only when the release was filmed. [`say_line`] folds to `usable` so the
/// arithmetic is done once, here, rather than in the head of whoever writes
/// the next line.
struct Layout {
    margin: String,
    usable: usize,
}

/// The margin a terminal of `width` columns gets, and the room left inside it.
///
/// One copy of the arithmetic, so the tests measure the budget the tour
/// actually prints against rather than a second-hand restatement of it.
fn budget(width: usize) -> (String, usize) {
    let margin = " ".repeat(2.max(width.saturating_sub(62) / 2));
    // A terminal narrower than the margin is not worth laying out for, but it
    // must not produce a zero-width budget that puts every word on its own
    // line.
    let usable = width.saturating_sub(margin.len()).max(20);
    (margin, usable)
}

fn layout() -> &'static Layout {
    static LAYOUT: std::sync::OnceLock<Layout> = std::sync::OnceLock::new();
    LAYOUT.get_or_init(|| {
        let width = console::Term::stderr()
            .size_checked()
            .map_or(80, |(_, cols)| cols as usize);
        let (margin, usable) = budget(width);
        // The installs the tour runs print from the shared install path,
        // which knows nothing about the tour and would otherwise hug the
        // terminal edge while the prose above it sat centred.
        crate::mochi::set_indent(margin.clone());
        Layout { margin, usable }
    })
}

fn margin() -> &'static str {
    &layout().margin
}

/// Fold `text` to `width` display columns, hanging the continuation under the
/// line's own leading indent.
///
/// Width is measured with [`console::measure_text_width`], not `len`, so the
/// `style(…)` escapes the tour prints everywhere are not counted as columns.
/// A line that already fits is returned untouched — folding is invisible to
/// every line that was never in trouble.
///
/// A style that spans a fold point survives it — the escape stays attached to
/// its word and the terminal carries the attribute across the newline to the
/// reset.
fn fold(text: &str, width: usize) -> Vec<String> {
    if console::measure_text_width(text) <= width {
        return vec![text.to_owned()];
    }
    // Bullets and continuation lines carry their own leading spaces; the
    // fold keeps them so a wrapped bullet hangs under itself rather than
    // sliding back to the margin.
    let body = text.trim_start_matches(' ');
    let indent = &text[..text.len() - body.len()];
    let inner = width.saturating_sub(console::measure_text_width(indent));
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in body.split(' ').filter(|word| !word.is_empty()) {
        for piece in split_word(word, inner) {
            if current.is_empty() {
                current = format!("{indent}{piece}");
            } else if console::measure_text_width(&current)
                + 1
                + console::measure_text_width(&piece)
                > width
            {
                lines.push(std::mem::take(&mut current));
                current = format!("{indent}{piece}");
            } else {
                current.push(' ');
                current.push_str(&piece);
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    // A line of nothing but spaces, longer than the budget, trims to no words
    // at all. Returning nothing would swallow the line and leave [`ask_prompt`]
    // with no last line to put the cursor after.
    if lines.is_empty() {
        lines.push(text.to_owned());
    }
    lines
}

/// Break a word that cannot fit the budget even on a line of its own.
///
/// Leaving such a word whole was the first design, and it left the promise
/// half-kept. The adopt act prints the install it found, and the statusline act
/// prints the command the reader already had — a real one is
/// `/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js`, 63
/// columns of unbreakable word — so on a narrow terminal the fragment still
/// landed at column 0: the exact failure all of this exists to prevent, just at
/// a width nobody had looked at. Splitting is the uglier of two ugly options
/// and the only one that keeps the block whole.
///
/// Escape sequences are stepped over whole rather than exempted. Exempting any
/// word carrying one left the guarantee broken exactly where it was most likely
/// to break: the statusline the reader already has is a long path AND it is
/// styled. Stepping over them means a break can never land inside a sequence,
/// while a style that spans one survives it the same way it does across a fold.
fn split_word(word: &str, width: usize) -> Vec<String> {
    if width == 0 || console::measure_text_width(word) <= width {
        return vec![word.to_owned()];
    }
    let mut pieces: Vec<String> = Vec::new();
    let mut piece = String::new();
    let mut painted = 0usize;
    let mut characters = word.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            piece.push(character);
            match characters.peek() {
                // CSI — parameters, then a letter. This is what `style(…)`
                // emits.
                Some('[') => {
                    for escape in characters.by_ref() {
                        piece.push(escape);
                        if escape.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                // OSC — a hyperlink, which runs to BEL or to ESC \. Its URL is
                // full of letters, so the CSI rule would stop inside it.
                Some(']') => {
                    let mut after_escape = false;
                    for escape in characters.by_ref() {
                        piece.push(escape);
                        if escape == '\u{7}' || (after_escape && escape == '\\') {
                            break;
                        }
                        after_escape = escape == '\u{1b}';
                    }
                }
                _ => {
                    if let Some(escape) = characters.next() {
                        piece.push(escape);
                    }
                }
            }
            continue;
        }
        let character_width = console::measure_text_width(character.encode_utf8(&mut [0; 4]));
        if painted > 0 && painted + character_width > width {
            pieces.push(std::mem::take(&mut piece));
            painted = 0;
        }
        piece.push(character);
        painted += character_width;
    }
    if !piece.is_empty() {
        pieces.push(piece);
    }
    pieces
}

/// Print one line of tour copy on the margin, folded to what is left.
fn say_line(text: &str) {
    for line in fold(text, layout().usable) {
        eprintln!("{}{line}", margin());
    }
}

/// Print a prompt on the margin, folded, leaving the cursor after the last
/// line so the answer is typed where the reader is looking.
fn ask_prompt(text: &str) {
    let lines = fold(text, layout().usable);
    let (last, rest) = lines.split_last().expect("fold always yields a line");
    for line in rest {
        eprintln!("{}{line}", margin());
    }
    eprint!("{}{last}", margin());
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

/// `eprintln!` on the tour margin, folded to the width inside it.
macro_rules! say {
    () => { eprintln!() };
    ($($arg:tt)*) => { say_line(&format!($($arg)*)) };
}

/// `eprint!` on the tour margin — for prompts that read on the same line.
macro_rules! ask {
    ($($arg:tt)*) => { ask_prompt(&format!($($arg)*)) };
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

/// The opening line, and the width it has to live inside.
///
/// The story block is centred on 62 columns and every act shares that margin,
/// so a line has `width - margin` columns before the terminal wraps it — and a
/// wrapped line does not wrap politely: the remainder lands at column 0,
/// outside the margin, breaking the centred block. On an 80-column terminal
/// that budget is 71 characters, which is the narrowest case worth holding to.
///
/// The line this replaced was 91 characters and wrapped on every terminal
/// width, including the 106-column grid the hero video records at — caught
/// only when it was filmed. Hence [`opening_summary_fits_a_narrow_terminal`].
const OPENING_SUMMARY: &str = "Hatching sets up Claude Code, Codex, and claudex (Pi optional).";

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
    say!("{OPENING_SUMMARY}");
    say!(
        "The story is why this exists — two cats and an echo. {} skips to setup.",
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
    // The summary is about to name `ccy` and its siblings, so put them on
    // disk first — see `shortcuts::install_for_tour`.
    shortcuts::install_for_tour();
    // Commands first, then the globe closes the show — the summary must not
    // be the thing on screen after "fin."
    print_summary(claude, codex, claudex, pi, false);
    story.wait_for_fin();
    story.fin();
    print_outro(claude, codex, claudex);
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
    shortcuts::install_for_tour();
    print_summary(claude, codex, claudex, pi, true);
    print_outro(claude, codex, claudex);
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
        command_row(&launch_command("ccy"), "Claude Code (yolo)");
    }
    if codex {
        command_row(&launch_command("cxy"), "Codex (yolo)");
    }
    if claudex {
        command_row(
            &launch_command("ccxy"),
            "claudex — Claude Code on GPT-5.6 (yolo)",
        );
    }
    if pi {
        command_row("ovm pi", "Pi");
    }
    command_row("ovm select", "browse, install, switch versions");
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

/// One command and what it does, with the descriptions aligned on a column
/// wide enough for the longest name the summary can print.
fn command_row(name: &str, description: &str) {
    let pad = " ".repeat(10usize.saturating_sub(name.len()));
    say_line(&format!("  {}{pad}  {description}", style(name).bold()));
}

/// What to tell the reader to type for a shortcut.
///
/// The bare shim when it is really there and really ours, the `ovm` subcommand
/// it wraps otherwise. The tour must never close by naming a command that does
/// not exist — which is exactly what it did while the shims lived in
/// `~/.local/bin` and nothing in the tour installed them.
fn launch_command(shim: &str) -> String {
    if shortcuts::shim_is_ready(shim) {
        shim.to_owned()
    } else {
        format!("ovm {shim}")
    }
}

/// Set by the installer when the shell it was launched from will not find
/// `ovm` after it exits.
///
/// The installer writes the PATH line into the shell rc, which reaches new
/// shells only: a child process cannot alter its parent's environment, because
/// `execve` hands the child a copy. It then injects `~/.ovm/bin` into the PATH
/// of the tour it launches — which is why the tour cannot answer this question
/// by looking at its own PATH, and has to be told.
const PATH_PENDING_ENV: &str = "OVM_PATH_PENDING";

/// Whether the shell waiting behind this tour will find these commands.
fn path_pending() -> bool {
    if std::env::var_os(PATH_PENDING_ENV).is_some_and(|value| !value.is_empty()) {
        return true;
    }
    // Run by hand rather than from the installer: this process resolved `ovm`
    // through the user's own PATH, so its own PATH is the honest answer.
    crate::config::OvmDirs::new().is_ok_and(|dirs| !shortcuts::dir_on_path(&dirs.bin))
}

/// The last word: what to type, and where it will work.
///
/// This is the one line the tour cannot afford to lose, and every other screen
/// is a poor place to put it — each scene clears the one before it, and on the
/// story path `fin.` is deliberately the final picture. So it prints beneath
/// whatever closed the show, where it is still on screen when the reader gets
/// their prompt back.
///
/// It went missing entirely in 0.1.7: the installer printed the PATH advice
/// before offering the tour, and the tour's opening screen-clear wiped it — on
/// the default answer, every time. The reader then landed back in a shell where
/// none of the commands the summary had just listed could be found.
fn print_outro(claude: bool, codex: bool, claudex: bool) {
    let ready: Vec<&str> = [("ccy", claude), ("cxy", codex), ("ccxy", claudex)]
        .into_iter()
        .filter(|(shim, set_up)| *set_up && shortcuts::shim_is_ready(shim))
        .map(|(shim, _)| shim)
        .collect();
    // Nothing to name — but the PATH problem is the reader's either way, and
    // this is still the last place to say so.
    if ready.is_empty() {
        if path_pending() {
            eprintln!();
            say!(
                "{} This shell started before the install, so it cannot see ovm yet.",
                style("◇").dim()
            );
            say!("  Open a new terminal session, or run here:");
            say!("  {}", style("export PATH=\"$HOME/.ovm/bin:$PATH\"").bold());
            eprintln!();
        }
        return;
    }
    eprintln!();
    say!(
        "{} Open a new terminal session, then try {}.",
        style("◇").dim(),
        style(and_list(&ready)).bold()
    );
    if path_pending() {
        say!("  This shell started before the install — to use them here:");
        say!("  {}", style("export PATH=\"$HOME/.ovm/bin:$PATH\"").bold());
    }
    // The tour installs Claude Code and never signs anyone in. That is
    // deliberate — first-run auth is Claude's own business, and staying
    // credential-free is what makes this whole flow testable — but the summary
    // above promises `ccy` works, and meeting that promise with an unannounced
    // login screen is a poor handoff. One line closes it, and names the
    // asymmetry a reader would otherwise trip over: claudex opened a browser
    // during setup, Claude never did.
    if ready.contains(&"ccy") {
        eprintln!();
        say!(
            "  First run of {} signs you in to Claude.",
            style("ccy").bold()
        );
        // Which ACCOUNT it runs on, not that a grant exists. The claudex act
        // can finish having declined the browser step, and the first version of
        // this line said "ccxy already has the ChatGPT account you connected"
        // on exactly that take — a promise the run had not kept.
        if ready.contains(&"ccxy") {
            say!(
                "  {} runs on your ChatGPT account instead.",
                style("ccxy").bold()
            );
        }
    }
    eprintln!();
}

/// `["ccy", "cxy", "ccxy"]` → `"ccy, cxy and ccxy"`.
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
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
        // Report it like every other act. This branch used to return in
        // silence, so on a machine that already had Claude the ledger held no
        // Claude act at all — and a missing act reads exactly like one that
        // never ran, which is the confusion the events sink exists to remove.
        // Only a re-run reaches here, so no fresh-machine test could see it.
        event(
            "install",
            "ok",
            &[("product", "claude"), ("version", &current)],
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
        style(format!("ovm {} latest", product.shortest_alias())).bold(),
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
        // Same teaching move as the hatch act: show the command, then run it.
        // A tour that installs things FOR you and never names the gesture
        // leaves you dependent on the tour — the reader should walk away able
        // to do this on a machine OVM has never seen.
        say!(
            "  {} {}",
            style("$").dim(),
            style(format!("ovm {} latest", product.shortest_alias())).bold()
        );
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
    say!(
        "  {} still hatches (~190MB). Getting there is one gesture — and",
        style(HATCH_VERSION).bold()
    );
    say!("  rather than describe it, OVM will do it once while you watch.");
    if !confirm_default_no("Hatch your own?")? {
        say!(
            "{} Skipped. Anytime: {} → Claude → {} for the versions that still have one.",
            style("→").dim(),
            style("ovm switch").bold(),
            style("b").bold(),
        );
        event("hatch", "skipped", &[]);
        return Ok(false);
    }

    // The gesture, performed rather than described. Printing the command the
    // way a shell would is the whole teaching move: what follows on screen is
    // what `ovm switch` looks like when THEY run it, because it is the real
    // picker with a scripted hand on the keys.
    eprintln!();
    say!(
        "{} Watch — this is the gesture, and next time it is yours:",
        style("→").dim()
    );
    eprintln!();
    say!("  {} {}", style("$").dim(), style("ovm switch").bold());
    std::thread::sleep(std::time::Duration::from_millis(1400));

    let selected = super::select::run_guided_buddy_switch()?;
    let Some(version) = selected else {
        // They took the keyboard back and left the picker. Not a failure —
        // just a tour that stops teaching and gets out of the way.
        say!(
            "{} Left the picker. Anytime: {} → Claude → {}.",
            style("→").dim(),
            style("ovm switch").bold(),
            style("b").bold(),
        );
        event("hatch", "skipped", &[("reason", "left the picker")]);
        return Ok(false);
    };

    eprintln!();
    say!(
        "{} Inside, type {} — then {} brings you back here.",
        style("→").dim(),
        style("/buddy").bold(),
        style("/exit").bold(),
    );
    say!(
        "  When you want the newest Claude Code again: {}",
        style("ovm cc latest").bold()
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
    cmd.args(["cc", "--ovm-version", &version]);
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
/// The tour's margin, and the fact that it has already introduced claudex.
/// Read by `ovm-claudex`'s own output layer — see its `output` module.
const CLAUDEX_INDENT_ENV: &str = "OVM_OUTPUT_INDENT";
const CLAUDEX_BRIEF_ENV: &str = "OVM_CLAUDEX_BRIEF";

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
    // Hand down the tour's left edge, and say that the introduction is already
    // made. The wizard is a separate process, so neither could reach it before:
    // it printed flush-left inside a centred tour and re-delivered the pitch
    // chapter iii had just given, which is most of what made the handoff feel
    // like leaving one program for another.
    cmd.env(CLAUDEX_INDENT_ENV, margin());
    cmd.env(CLAUDEX_BRIEF_ENV, "1");
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
        say!("  Kept as is unless you want Echo instead — backed up either way.");
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
    read_confirm_key(true)
}

/// A prompt whose safe answer is "no": the optional acts must not run
/// themselves when someone leans on Enter through the tour.
fn confirm_default_no(question: &str) -> Result<bool> {
    ask!("{} {question} [y/N] ", style("›").yellow().bold());
    io::stderr().flush()?;
    read_confirm_key(false)
}

/// One keypress answers a [Y/n] prompt — `y`, `n`, or Enter for the default —
/// with no Enter after it. The terminal is in raw mode for the read, so the
/// answer is echoed here (with the newline that ends the prompt line);
/// Escape reads as "no", and any other key is ignored rather than guessed at.
/// Ctrl-C still quits: `read_key` restores the terminal and raises SIGINT on
/// itself, so the tour ends exactly as it did from a line-reading prompt.
///
/// Falls back to reading a line whenever a keypress cannot be read. `read_key`
/// reports a non-terminal stderr as `Key::Unknown` rather than as an error —
/// it does not block, so ignoring it as "some other key" would spin forever on
/// `ovm hatch 2>file`. The line path is what every prompt did before this, so
/// a redirected or scripted run behaves exactly as it used to.
fn read_confirm_key(default_yes: bool) -> Result<bool> {
    let term = console::Term::stderr();
    if !term.is_term() {
        return read_confirm_line(default_yes);
    }
    loop {
        let answer = match term
            .read_key()
            .map_err(|e| OvmError::Message(e.to_string()))?
        {
            console::Key::Enter => default_yes,
            console::Key::Char('y' | 'Y') => true,
            console::Key::Char('n' | 'N') | console::Key::Escape => false,
            console::Key::Unknown => return read_confirm_line(default_yes),
            _ => continue,
        };
        eprintln!("{}", if answer { "y" } else { "n" });
        return Ok(answer);
    }
}

/// The line-reading answer: `y`/`yes`, `n`/`no`, or Enter for the default.
fn read_confirm_line(default_yes: bool) -> Result<bool> {
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    if answer.is_empty() {
        return Ok(default_yes);
    }
    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {

    /// The opening line must survive an 80-column terminal on one line.
    ///
    /// [`fold`] means an over-long line no longer breaks the layout, so this
    /// is no longer the thing standing between the tour and a broken screen —
    /// it is a copy budget. The first screen reads better as two whole lines
    /// than as two-and-a-fragment, and 71 columns is what an 80-column
    /// terminal leaves inside the margin. Measured as display width, so the
    /// day someone wraps this line in `style(…)` the escapes are not counted
    /// as columns.
    #[test]
    fn opening_summary_fits_a_narrow_terminal() {
        let (_, usable) = super::budget(80);
        let width = console::measure_text_width(super::OPENING_SUMMARY);
        assert!(
            width <= usable,
            "the opening line is {width} columns; an 80-column terminal \
             fits {usable} inside the margin",
        );
    }

    #[test]
    fn and_list_reads_as_a_sentence() {
        assert_eq!(and_list(&[]), "");
        assert_eq!(and_list(&["ccy"]), "ccy");
        assert_eq!(and_list(&["ccy", "cxy"]), "ccy and cxy");
        assert_eq!(and_list(&["ccy", "cxy", "ccxy"]), "ccy, cxy and ccxy");
    }

    /// The summary's descriptions line up whatever mix of bare shims and
    /// `ovm …` fallbacks it ends up printing.
    #[test]
    fn command_rows_align_on_one_column() {
        for name in ["ccy", "ccxy", "ovm pi", "ovm select"] {
            let pad = " ".repeat(10usize.saturating_sub(name.len()));
            assert_eq!(name.len() + pad.len(), 10, "{name} breaks the column");
        }
    }

    /// The whole point of folding: a line that fits is printed as it was.
    ///
    /// Every line of the tour goes through [`fold`], and the recording is cut
    /// at 106 columns where all of the copy fits. If folding perturbed a line
    /// that was never in trouble, it would rewrite the screen the hero video
    /// is filmed against.
    #[test]
    fn fold_leaves_a_line_that_fits_untouched() {
        let line = "  Kept as is unless you want Echo instead — backed up either way.";
        assert_eq!(super::fold(line, 71), vec![line.to_string()]);
    }

    /// A folded line hangs under its own indent, not back at the margin.
    ///
    /// This is the failure that shipped in 0.1.7, in miniature: the remainder
    /// of a wrapped line landing to the LEFT of the block it belongs to is
    /// what made the first screen look broken.
    #[test]
    fn fold_hangs_the_continuation_under_the_indent() {
        let folded = super::fold("  one two three four five", 12);
        assert_eq!(folded, vec!["  one two", "  three four", "  five"]);
    }

    /// Width is columns, not bytes: styling must not spend the budget.
    ///
    /// `style(…)` wraps most of the tour's emphasis, and an escape sequence is
    /// several bytes wide and zero columns wide. Measuring bytes would fold
    /// lines that fit perfectly well.
    #[test]
    fn fold_measures_display_width_not_bytes() {
        let styled = "\u{1b}[1mbold\u{1b}[0m words here";
        assert!(styled.len() > 20, "the escapes make this long in bytes");
        assert_eq!(super::fold(styled, 20), vec![styled.to_string()]);
    }

    /// A word too long for any line is split rather than left to overhang.
    ///
    /// The adopt act prints the install it found, and a real Homebrew path is
    /// 63 columns of unbreakable word — wider than the budget on a narrow
    /// terminal. Left whole it hangs past the edge and the remainder lands at
    /// column 0, which is the failure this whole arrangement exists to stop.
    #[test]
    fn fold_splits_a_word_that_cannot_fit_a_line() {
        let path = "/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js";
        let folded = super::fold(&format!("  Found it at {path}"), 30);
        for line in &folded {
            assert!(
                console::measure_text_width(line) <= 30,
                "{line:?} is wider than the budget it was folded to",
            );
        }
        let rejoined: String = folded.iter().map(|line| line.trim_start()).collect();
        assert!(
            rejoined.contains("claude-code/cli.js"),
            "the path survived: {rejoined}"
        );
    }

    /// A styled word is split without ever cutting an escape sequence.
    ///
    /// The statusline act prints the command the reader already had, styled
    /// dim — commonly a long path. Exempting styled words from the split left
    /// the width guarantee broken in the one place most likely to break it.
    #[test]
    fn fold_splits_a_styled_word_without_cutting_the_escape() {
        let styled = format!("\u{1b}[2m{}\u{1b}[0m", "x".repeat(40));
        let pieces = super::split_word(&styled, 10);
        assert!(pieces.len() > 1, "a 40-column word does not fit 10 columns");
        for piece in &pieces {
            assert!(
                console::measure_text_width(piece) <= 10,
                "{piece:?} is wider than the width it was split to",
            );
        }
        assert_eq!(
            pieces.concat(),
            styled,
            "splitting must not lose or duplicate a single byte",
        );
    }

    /// Folding never returns nothing, so no line is silently swallowed.
    ///
    /// A line of nothing but spaces, longer than the budget, has no words to
    /// fold. Returning an empty list would drop it, and would leave
    /// `ask_prompt` with no last line to put the cursor after — a panic in the
    /// middle of onboarding.
    #[test]
    fn fold_keeps_a_line_that_has_no_words_to_fold() {
        let spaces = " ".repeat(40);
        assert_eq!(super::fold(&spaces, 10), vec![spaces]);
    }

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

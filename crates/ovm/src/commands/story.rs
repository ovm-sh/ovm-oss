//! `ovm story` — a tale of two cats and an echo.
//!
//! An interactive terminal story: where Quelpaw came from, how Mochi got
//! here, what Echo really is — ending in the fin. All of it true, all of it
//! recovered. The story only hatches for the command that started it all:
//! the user has to type `/buddy` at the title screen.
//!
//! `--fast` (or a non-tty on either end) plays straight through without
//! waiting for input.

use crate::error::Result;
use std::io::{self, IsTerminal, Write};
use std::thread::sleep;
use std::time::Duration;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
/// SGR 24 ends the underline without clearing colour, so an underlined label
/// can sit inside a coloured line (the "buy a plate" line is MAGENTA).
const UNDERLINE_OFF: &str = "\x1b[24m";
const MAGENTA: &str = "\x1b[35m";
const ORANGE: &str = "\x1b[38;5;215m";
const RUST: &str = "\x1b[38;5;130m";
const GREY: &str = "\x1b[38;5;245m";
/// Quelpaw's true coat — the original buddy was grey.
const SILVER: &str = "\x1b[38;5;250m";

// ---- the cast ---------------------------------------------------------------

const MOCHI: [&str; 3] = [r"   /\_/\   ", r"  ( ^.^ )  ", r"   > ^ <   "];
/// Mochi's blink and wink, using the same faces OVM shows everywhere else
/// (mochi.rs: `-.-` is the working face, `o.o` the default). Reusing that
/// vocabulary keeps one cat across the CLI and the story instead of two.
const MOCHI_BLINK: [&str; 3] = [r"   /\_/\   ", r"  ( -.- )  ", r"   > ^ <   "];
const MOCHI_WINK: [&str; 3] = [r"   /\_/\   ", r"  ( ^.- )  ", r"   > ^ <   "];
const MOCHI_LOOK: [&str; 3] = [r"   /\_/\   ", r"  ( o.o )  ", r"   > ^ <   "];
const QUELPAW: [&str; 4] = [
    r"  /\    /\  ",
    r" ( @    @ ) ",
    r" (   ..   ) ",
    r"  `------´  ",
];
const QUELPAW_EAR: [&str; 4] = [
    r"  /\    /|  ",
    r" ( @    @ ) ",
    r" (   ..   ) ",
    r"  `------´  ",
];
const QUELPAW_TAIL: [&str; 4] = [
    r"  /\    /\  ",
    r" ( @    @ ) ",
    r" (   ..   ) ",
    r"  `------´~ ",
];
/// Removed in 2.1.97. The ending screen shows this one: eyes crossed, ear
/// still notched, and no idle loop — a cat that has stopped moving.
const QUELPAW_GONE: [&str; 4] = [
    r"  /\    /\  ",
    r" ( x    x ) ",
    r" (   ..   ) ",
    r"  `------´  ",
];
/// The happy pet pose, hearts row first — the fin lineup drops the hearts.
const QUELPAW_PET: [&str; 5] = [
    r"   ♥    ♥   ",
    r"  /\    /\  ",
    r" ( ^    ^ ) ",
    r" (   ww   ) ",
    r"  `------´  ",
];
const ECHO: [&str; 4] = [
    r"  /\    /|  ",
    r" ( O    O ) ",
    r" (   oo   ) ",
    r"  `------´~ ",
];
/// Echo naps when you idle and flicks their tail at your context window —
/// so the idle loop is exactly those two things.
const ECHO_NAP: [&str; 4] = [
    r"  /\    /|  ",
    r" ( -    - ) ",
    r" (   oo   ) ",
    r"  `------´~ ",
];
const ECHO_TAIL: [&str; 4] = [
    r"  /\    /|  ",
    r" ( O    O ) ",
    r" (   oo   ) ",
    r"  `------´  ",
];

// ---- the Atlas sphere (ported from brand-assets/sphere; house recipe) -------

const RAMP: &[u8] = b".:+*#@";
const LIGHT: (f64, f64, f64) = (-0.55, -0.4, 0.73);
const AMB: f64 = 0.12;
const GAMMA: f64 = 0.9;
const SHADES: [&str; 5] = [
    "\x1b[38;5;240m",
    "\x1b[38;5;243m",
    "\x1b[38;5;247m",
    "\x1b[38;5;251m",
    "\x1b[38;5;255m",
];
const DECAL_INK: &str = "\x1b[1m\x1b[38;5;231m";
// Terminal cells are taller than 2:1 — 29 keeps the sphere round, not egg.
const RY: i32 = 12;
const RX: i32 = 29;
const GRID_H: usize = (RY * 2 + 1) as usize;
const DECAL_SPAN: f64 = 100.0;

fn glyph_3x5(letter: char) -> [&'static str; 5] {
    match letter {
        'A' => ["111", "101", "111", "101", "101"],
        'T' => ["111", "010", "010", "010", "010"],
        'L' => ["100", "100", "100", "100", "111"],
        'S' => ["111", "100", "111", "001", "111"],
        'C' => ["111", "100", "100", "100", "111"],
        'O' => ["111", "101", "101", "101", "111"],
        'D' => ["110", "101", "101", "101", "110"],
        'E' => ["111", "100", "111", "100", "111"],
        _ => ["000", "000", "000", "000", "000"],
    }
}

fn word_rows(word: &str) -> Vec<String> {
    let mut rows = vec![String::new(); 5];
    let last = word.chars().count() - 1;
    for (i, letter) in word.chars().enumerate() {
        let glyph = glyph_3x5(letter);
        for (r, row) in rows.iter_mut().enumerate() {
            row.push_str(glyph[r]);
            if i < last {
                row.push('0');
            }
        }
    }
    rows
}

fn decal_mask() -> Vec<String> {
    let mut mask = word_rows("ATLAS");
    mask.push("0".repeat(19));
    mask.extend(word_rows("CODES"));
    mask
}

fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

/// Render one longitude of the spinning decal sphere as terminal lines.
fn sphere_frame(mask: &[String], lon_center: f64, indent: usize) -> Vec<String> {
    let decal_rows = mask.len();
    let decal_cols = mask[0].chars().count();
    let decal_h = decal_rows as f64 + 0.5;
    let (lx, ly, lz) = LIGHT;
    let llen = (lx * lx + ly * ly + lz * lz).sqrt();

    let mut lines = Vec::with_capacity(GRID_H);
    for row in 0..GRID_H {
        let y = row as i32 - RY;
        let ny = f64::from(y) / (f64::from(RY) + 0.5);
        let mut out = String::new();
        for col in 0..(RX * 2 + 1) {
            let x = col - RX;
            let nx = f64::from(x) / (f64::from(RX) + 0.5);
            let r2 = nx * nx + ny * ny;
            if r2 > 1.0 {
                out.push(' ');
                continue;
            }
            let nz = (1.0 - r2).max(0.0).sqrt();
            let mut b =
                (nx * lx / llen + ny * ly / llen + nz * lz / llen).max(0.0) * (1.0 - AMB) + AMB;
            b = b.clamp(0.0, 1.0).powf(GAMMA);
            let ramp_i = ((b * RAMP.len() as f64) as usize).min(RAMP.len() - 1);
            let shade_i = ((b * SHADES.len() as f64) as usize).min(SHADES.len() - 1);
            let mut decal_hit = false;
            if f64::from(y).abs() <= decal_h / 2.0 {
                let phi = nx.clamp(-1.0, 1.0).asin().to_degrees();
                let mut u = (phi - lon_center) % 360.0;
                if u < 0.0 {
                    u += 360.0;
                }
                if u > 180.0 {
                    u -= 360.0;
                }
                if u.abs() <= DECAL_SPAN / 2.0 {
                    let dc =
                        (((u / DECAL_SPAN + 0.5) * decal_cols as f64) as usize).min(decal_cols - 1);
                    let dr = (((f64::from(y) / decal_h + 0.5) * decal_rows as f64) as usize)
                        .min(decal_rows - 1);
                    if mask[dr].chars().nth(dc) == Some('1') {
                        decal_hit = true;
                    }
                }
            }
            if decal_hit {
                out.push_str(DECAL_INK);
                out.push('█');
                out.push_str(RESET);
            } else {
                out.push_str(SHADES[shade_i]);
                out.push(RAMP[ramp_i] as char);
                out.push_str(RESET);
            }
        }
        lines.push(format!("{}{}", " ".repeat(indent), out));
    }
    lines
}

/// Whether typed input opens the story at the /buddy gate.
fn accepts_buddy(input: &str) -> bool {
    matches!(input.trim().to_lowercase().as_str(), "/buddy" | "buddy")
}

/// Length of the escape sequence starting at `start`, or `None` if the
/// character there is ordinary text.
///
/// The typewriter paces *visible* characters. Without this it would sleep
/// once per byte of an invisible sequence — an OSC 8 hyperlink is ~40 bytes,
/// so a linked line took half a second of dead air before its first letter
/// appeared. The old workaround was to print link lines instantly, which is
/// what made chapter ii read as snap, snap, type, type, type, snap, snap.
///
/// Two forms appear in this story:
///   CSI  `ESC [ … final`   final byte in `@`..=`~`  (colour, underline)
///   OSC  `ESC ] … ST`      terminated by `ESC \` or BEL  (hyperlinks)
fn escape_run(chars: &[char], start: usize) -> Option<usize> {
    if chars.get(start) != Some(&'\x1b') {
        return None;
    }
    let mut i = start + 1;
    match chars.get(i) {
        Some('[') => {
            i += 1;
            while let Some(&c) = chars.get(i) {
                i += 1;
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
        Some(']') => {
            i += 1;
            while let Some(&c) = chars.get(i) {
                i += 1;
                if c == '\x07' {
                    break;
                }
                if c == '\x1b' {
                    if chars.get(i) == Some(&'\\') {
                        i += 1;
                    }
                    break;
                }
            }
        }
        // A lone two-character escape (including the bare ST that closes OSC 8).
        Some(_) => i += 1,
        None => {}
    }
    Some(i - start)
}

// ---- the teller -------------------------------------------------------------

struct Story {
    fast: bool,
    width: usize,
}

impl Story {
    fn beat(&self, ms: u64) {
        if !self.fast {
            sleep(Duration::from_millis(ms));
        }
    }

    fn margin(&self) -> String {
        " ".repeat(2.max((self.width.saturating_sub(62)) / 2))
    }

    fn center(&self, text: &str, visible: usize) -> String {
        format!(
            "{}{}",
            " ".repeat(self.width.saturating_sub(visible) / 2),
            text
        )
    }

    /// Typewriter line, centered-ish left margin for prose.
    fn say(&self, text: &str, color: &str, pace_ms: u64, lead_ms: u64) {
        self.beat(lead_ms);
        let margin = self.margin();
        if self.fast || pace_ms == 0 {
            println!(
                "{margin}{color}{text}{}",
                if color.is_empty() { "" } else { RESET }
            );
            return;
        }
        print!("{margin}{color}");
        let mut out = io::stdout();
        // Escape sequences are emitted whole and instantly; only the letters
        // the reader can actually see cost a beat. That lets a line carrying a
        // hyperlink type at the same rhythm as the prose around it.
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if let Some(run) = escape_run(&chars, i) {
                for ch in &chars[i..i + run] {
                    print!("{ch}");
                }
                let _ = out.flush();
                i += run;
                continue;
            }
            print!("{}", chars[i]);
            let _ = out.flush();
            sleep(Duration::from_millis(pace_ms));
            i += 1;
        }
        println!("{}", if color.is_empty() { "" } else { RESET });
    }

    fn blank(&self) {
        println!();
    }

    /// Reveal the first frame line by line, then rewrite the same lines in
    /// place for every frame after it.
    ///
    /// The reveal matters: `art()` draws line by line with a beat between, so a
    /// static cat appears to arrive, while an animated one used to slam its
    /// whole first frame down in a single write and then flick subtly. Side by
    /// side, the animated cat looked like the still one — the opposite of the
    /// intent. Both entrances now match; only what happens next differs.
    fn animate(&self, frame_sets: &[&[&str]], color: &str, loops: usize, hold_ms: u64) {
        let height = frame_sets[0].len();
        let widest = frame_sets
            .iter()
            .flat_map(|f| f.iter())
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0);
        let left = " ".repeat(self.width.saturating_sub(widest) / 2);
        let mut first = true;
        let mut out = io::stdout();
        for _ in 0..loops {
            for frames in frame_sets {
                if !first {
                    print!("\x1b[{height}F");
                }
                for line in frames.iter() {
                    println!("\x1b[2K{left}{color}{line}{RESET}");
                    // Only the very first frame is revealed; redraws must stay
                    // instant or the flicks turn into a stutter.
                    if first {
                        let _ = out.flush();
                        self.beat(80);
                    }
                }
                let _ = out.flush();
                first = false;
                self.beat(hold_ms);
            }
        }
        self.beat(200);
    }

    fn wait(&self) {
        if self.fast {
            return;
        }
        // The prompt gets its own air. Sitting flush against the last line of
        // narration it read as part of the sentence rather than a control, on
        // every chapter — so the blank belongs here, once, instead of at each
        // call site where it can be forgotten.
        self.blank();
        let prompt = format!(
            "{}{DIM}· enter ·{RESET}",
            " ".repeat(2.max(self.width.saturating_sub(11) / 2))
        );
        print!("{prompt}");
        let _ = io::stdout().flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_ok() {
            print!("\x1b[F\x1b[2K");
            let _ = io::stdout().flush();
        }
    }

    /// A terminal hyperlink (OSC 8): ctrl/cmd-click opens it, exactly like a
    /// link anywhere else in the terminal.
    ///
    /// This replaced a keyboard mechanic — the prompt used to read
    /// "· enter · o opens <label> ·" and `o` shelled out to `open`. It made the
    /// reader learn a story-specific key for something their terminal already
    /// does, and it put machinery in a line that should just be a sentence.
    ///
    /// Terminals that don't support OSC 8 ignore the sequence and print the
    /// text, so the line still reads correctly — never as raw escapes.
    ///
    /// The label is underlined (SGR 4, closed with 24 rather than a full
    /// reset so the caller's colour survives the link). Without it the link
    /// is invisible until the reader happens to hold cmd/ctrl — the text
    /// looked like prose, so nobody knew there was anything to click.
    fn link(&self, url: &str, text: &str) -> String {
        format!("\x1b]8;;{url}\x1b\\{UNDERLINE}{text}{UNDERLINE_OFF}\x1b]8;;\x1b\\")
    }

    /// Every chapter opens on a clean screen.
    fn fresh_page(&self) {
        if !self.fast {
            print!("\x1b[2J\x1b[H");
            let _ = io::stdout().flush();
        }
    }

    // ---- chapters -----------------------------------------------------------

    fn title(&self) {
        self.fresh_page();
        self.blank();
        println!("{}", self.center(&format!("{GREY}ovm presents{RESET}"), 12));
        self.blank();
        println!(
            "{}",
            self.center(&format!("{BOLD}a tale of two cats and an echo{RESET}"), 30)
        );
        self.blank();
        self.buddy_gate();
    }

    /// The story only hatches for the command that started it all.
    fn buddy_gate(&self) {
        let margin = self.margin();
        if self.fast {
            self.say("❯ /buddy", BOLD, 50, 500);
        } else {
            println!(
                "{}",
                self.center(&format!("{DIM}· type /buddy to begin ·{RESET}"), 24)
            );
            self.blank();
            loop {
                print!("{margin}{BOLD}❯ {RESET}");
                let _ = io::stdout().flush();
                let mut line = String::new();
                if io::stdin().read_line(&mut line).is_err() || line.is_empty() {
                    break;
                }
                if accepts_buddy(&line) {
                    break;
                }
                println!("{margin}{DIM}nothing hatches.{RESET}");
            }
        }
        self.blank();
        println!("{margin}{DIM}{ITALIC}◆ hatching a small creature that watches you code…{RESET}");
        self.beat(1200);
    }

    fn chapter_quelpaw(&self) {
        self.fresh_page();
        self.blank();
        println!("{}", self.center(&format!("{SILVER}i. quelpaw{RESET}"), 10));
        self.blank();
        // The recovered idle animation: ear flick, rest, tail flick — as archived.
        let loops = if self.fast { 1 } else { 2 };
        self.animate(
            &[&QUELPAW, &QUELPAW_EAR, &QUELPAW, &QUELPAW_TAIL],
            SILVER,
            loops,
            350,
        );
        self.blank();
        self.say(
            "Claude Code 2.1.80 shipped a secret. A slash command: /buddy.",
            "",
            12,
            0,
        );
        self.say("Nine releases on, the changelog owned up to it:", "", 12, 0);
        self.blank();
        let quote = format!("{ITALIC}{GREY}");
        self.say(
            "“/buddy is here for April 1st — hatch a small creature",
            &quote,
            10,
            0,
        );
        self.say(" that watches you code”", &quote, 10, 0);
        self.blank();
        self.say(
            "Every user hatched their own. Mine chose the curious",
            "",
            10,
            0,
        );
        self.say(
            "name of Quelpaw. Some said theirs helped them find bugs.",
            "",
            10,
            0,
        );
        self.blank();
        self.say(
            "For me, it was just nice to have them around.",
            SILVER,
            12,
            400,
        );
        self.wait();

        // The ending gets a clean screen, but Quelpaw stays on it. Clearing
        // everything (heading and cat included) read as a new chapter starting
        // rather than this one landing; keeping the cat above the last four
        // lines is what makes the goodbye theirs.
        self.fresh_page();
        self.blank();
        println!("{}", self.center(&format!("{SILVER}i. quelpaw{RESET}"), 10));
        self.blank();
        // Revealed line by line like every other cat, but a single frame and
        // no idle loop: this is the screen where they were removed, so they
        // arrive already still. The ear flick would undo the whole point.
        self.animate(&[&QUELPAW_GONE], SILVER, 1, 0);
        self.blank();
        self.say(
            "Seventeen releases they rode along. Then 2.1.97 removed them.",
            "",
            12,
            300,
        );
        self.say("And just like that they were gone.", "", 12, 0);
        self.blank();
        self.say(
            "We wanted to keep 2.1.96; and this is why OVM exists.",
            "",
            10,
            0,
        );
        self.say("The cat stays.", "", 10, 0);
        self.wait();
    }

    fn chapter_mochi(&self) {
        self.fresh_page();
        self.blank();
        println!("{}", self.center(&format!("{MAGENTA}ii. mochi{RESET}"), 9));
        self.blank();
        // Mochi is the relaxed one. `animate` holds every frame for the same
        // beat, so dwell is expressed by repeating a frame: Mochi rests, blinks
        // once, rests, glances over, settles, then winks and holds it. One
        // slower pass rather than two brisk ones — this cat is not in a hurry.
        self.animate(
            &[
                &MOCHI,
                &MOCHI,
                &MOCHI_BLINK,
                &MOCHI,
                &MOCHI_LOOK,
                &MOCHI,
                &MOCHI,
                &MOCHI_WINK,
                &MOCHI_WINK,
                &MOCHI,
            ],
            MAGENTA,
            1,
            460,
        );
        self.blank();
        // localaicat.com is the public site on purpose: the source repo
        // local-ai-cat/Local-AI-Chat is private, so linking it the way the
        // ship of theseus line does would hand every reader a 404.
        self.say(
            &format!(
                "Mochi came first, though — they were born in {},",
                self.link("https://localaicat.com", "local ai chat")
            ),
            "",
            12,
            0,
        );
        self.say(
            &format!(
                "but raised by the crew in the {}",
                self.link(
                    "https://github.com/shoreless/ship-of-theseus",
                    "ship of theseus"
                )
            ),
            "",
            12,
            0,
        );
        self.blank();
        self.say(
            "When Codex could grow a pet of its own, we asked for Mochi again —",
            "",
            10,
            0,
        );
        self.say(
            "redrawn from memory, sprite by sprite. And we brought them",
            "",
            10,
            0,
        );
        self.say(
            "back in ASCII — to greet every ovm and cxy user.",
            "",
            10,
            0,
        );
        self.blank();
        self.say(
            &format!(
                "Learn about ccy and cxy at {}",
                self.link("https://mochiexists.com/yolo/", "mochiexists.com/yolo")
            ),
            "",
            10,
            300,
        );
        // Two offers, not a list: the blank line keeps "buy a plate" from
        // reading as a second bullet of the ccy/cxy line above it.
        self.blank();
        self.say(
            &format!(
                "Buy a plate at {}",
                self.link("https://mochiexists.com/plate/", "mochiexists.com/plate")
            ),
            MAGENTA,
            10,
            0,
        );
        self.wait();
    }

    fn chapter_echo(&self) {
        self.fresh_page();
        self.blank();
        println!("{}", self.center(&format!("{ORANGE}iii. echo{RESET}"), 9));
        self.blank();
        let loops = if self.fast { 1 } else { 2 };
        self.animate(&[&ECHO, &ECHO_TAIL, &ECHO, &ECHO_NAP], ORANGE, loops, 350);
        self.blank();
        self.say(
            "When Quelpaw vanished, Codex tried to bring them back.",
            "",
            12,
            0,
        );
        self.blank();
        self.say(
            "It never read Anthropic's code. It only ever saw them —",
            "",
            10,
            0,
        );
        self.say("and drew what it could.", "", 10, 0);
        self.blank();
        self.say("Not a copy. An oscillation: Echo.", ORANGE, 12, 400);
        self.blank();
        self.say(
            "They live in the statusline now — reading the session's mood,",
            "",
            12,
            0,
        );
        self.say(
            "napping when you idle, flicking their tail at your context window.",
            "",
            12,
            0,
        );
        self.wait();
    }

    fn fin(&self) {
        self.fresh_page();
        self.blank();
        let mask = decal_mask();
        let indent = self.width.saturating_sub((RX * 2 + 1) as usize) / 2;
        let steps = if self.fast { 3 } else { 42 };
        let mut first = true;
        let mut out = io::stdout();
        for i in 0..steps {
            let t = i as f64 / (steps - 1).max(1) as f64;
            let lon = 200.0 * (1.0 - ease_out(t));
            let frame = sphere_frame(&mask, lon, indent);
            if !first {
                print!("\x1b[{GRID_H}F");
            }
            for line in &frame {
                println!("\x1b[2K{line}");
            }
            let _ = out.flush();
            first = false;
            self.beat(55);
        }
        self.beat(800);
        println!(
            "{}",
            self.center(&format!("{GREY}a t l a s   c o d e s{RESET}"), 21)
        );
        self.blank();
        self.beat(900);

        let quelpaw_fin: Vec<&str> = QUELPAW_PET[1..].to_vec();
        let mochi_fin: Vec<&str> = std::iter::once("           ")
            .chain(MOCHI.iter().copied())
            .collect();
        let echo_fin: Vec<&str> = ECHO.to_vec();
        let cats = [&quelpaw_fin, &mochi_fin, &echo_fin];
        let colors = [SILVER, MAGENTA, ORANGE];
        let names = ["quelpaw", "mochi", "echo"];
        let widths: Vec<usize> = cats
            .iter()
            .map(|cat| cat.iter().map(|l| l.chars().count()).max().unwrap_or(0))
            .collect();
        let gap = "      ";
        let block: usize = widths.iter().sum::<usize>() + gap.len() * 2;
        let left = " ".repeat(self.width.saturating_sub(block) / 2);
        for line_index in 0..4 {
            let row = cats
                .iter()
                .zip(&colors)
                .zip(&widths)
                .map(|((cat, color), w)| {
                    let line = cat[line_index];
                    let pad = " ".repeat(w.saturating_sub(line.chars().count()));
                    format!("{color}{line}{pad}{RESET}")
                })
                .collect::<Vec<_>>()
                .join(gap);
            println!("{left}{row}");
            self.beat(100);
        }
        let name_row = names
            .iter()
            .zip(&colors)
            .zip(&widths)
            .map(|((name, color), w)| {
                let total_pad = w.saturating_sub(name.chars().count());
                let lead = total_pad / 2;
                format!(
                    "{color}{}{name}{}{RESET}",
                    " ".repeat(lead),
                    " ".repeat(total_pad - lead)
                )
            })
            .collect::<Vec<_>>()
            .join(gap);
        println!("{left}{name_row}");
        self.blank();
        self.beat(600);
        println!(
            "{}",
            self.center(&format!("{DIM}built by mochi, echo and quelpaw{RESET}"), 32)
        );
        self.beat(250);
        println!(
            "{}",
            self.center(&format!("{DIM}tiny paws for big version jumps{RESET}"), 31)
        );
        self.blank();
        self.beat(900);
        println!("{}", self.center(&format!("{BOLD}{RUST}f i n .{RESET}"), 7));
        self.blank();
        self.beat(700);
        println!("{}", self.center(&format!("{GREY}ovm.sh{RESET}"), 6));
        self.blank();
    }
}

fn terminal_width() -> usize {
    console::Term::stdout()
        .size_checked()
        .map_or(80, |(_, cols)| cols as usize)
}

pub fn run(fast: bool) -> Result<()> {
    let fast = fast || !io::stdout().is_terminal() || !io::stdin().is_terminal();
    let story = Story {
        fast,
        width: terminal_width(),
    };
    story.title();
    story.chapter_quelpaw();
    story.chapter_mochi();
    story.chapter_echo();
    story.fin();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buddy_gate_accepts_the_command_in_any_dress() {
        assert!(accepts_buddy("/buddy"));
        assert!(accepts_buddy("  /BUDDY  "));
        assert!(accepts_buddy("buddy"));
        assert!(!accepts_buddy(""));
        assert!(!accepts_buddy("/help"));
        assert!(!accepts_buddy("bud"));
    }

    /// A link that is not underlined is invisible until the reader happens to
    /// hold cmd/ctrl — it reads as prose, which is how the local ai chat and
    /// ship of theseus links went unnoticed. The label must carry SGR 4, and
    /// must close with SGR 24 rather than a full reset so an underlined label
    /// can sit inside a coloured line without clearing the colour.
    #[test]
    fn links_are_underlined_and_do_not_clear_the_surrounding_colour() {
        let story = Story {
            fast: true,
            width: 80,
        };
        let rendered = story.link("https://localaicat.com", "local ai chat");

        assert!(rendered.contains("\x1b]8;;https://localaicat.com\x1b\\"));
        assert!(rendered.contains("\x1b[4mlocal ai chat\x1b[24m"));
        assert!(rendered.ends_with("\x1b]8;;\x1b\\"));
        // A full reset here would drop the caller's colour mid-line.
        assert!(!rendered.contains(RESET));
    }

    /// The typewriter must pace visible letters only. If `escape_run` ever
    /// stops recognising a sequence, the reader watches ~40 bytes of an OSC 8
    /// hyperlink tick by one sleep at a time before the line appears.
    #[test]
    fn escape_run_measures_sequences_and_leaves_prose_alone() {
        let story = Story {
            fast: true,
            width: 80,
        };
        let line = format!(
            "born in {},",
            story.link("https://localaicat.com", "local ai chat")
        );
        let chars: Vec<char> = line.chars().collect();

        // Ordinary prose is never mistaken for an escape.
        assert_eq!(escape_run(&chars, 0), None);

        // Every escape is consumed whole, and what is left is exactly the
        // text a reader sees — which is what the typewriter paces.
        let mut visible = String::new();
        let mut i = 0;
        while i < chars.len() {
            match escape_run(&chars, i) {
                Some(run) => i += run,
                None => {
                    visible.push(chars[i]);
                    i += 1;
                }
            }
        }
        assert_eq!(visible, "born in local ai chat,");

        // CSI underline, and the bare ST that closes a hyperlink.
        assert_eq!(
            escape_run(&"\x1b[4m".chars().collect::<Vec<_>>(), 0),
            Some(4)
        );
        assert_eq!(
            escape_run(&"\x1b\\".chars().collect::<Vec<_>>(), 0),
            Some(2)
        );
    }

    #[test]
    fn decal_mask_spells_two_rows_of_letters() {
        let mask = decal_mask();
        // 5 rows ATLAS + 1 spacer + 5 rows CODES, all 19 columns wide.
        assert_eq!(mask.len(), 11);
        assert!(mask.iter().all(|row| row.chars().count() == 19));
        // The spacer row carries no ink.
        assert!(mask[5].chars().all(|c| c == '0'));
        // The letter rows do.
        assert!(mask[0].contains('1'));
        assert!(mask[6].contains('1'));
    }

    #[test]
    fn sphere_frame_fills_the_grid() {
        let mask = decal_mask();
        let frame = sphere_frame(&mask, 0.0, 0);
        assert_eq!(frame.len(), GRID_H);
        // The equator row is the widest slice of the sphere and must carry ink.
        assert!(frame[GRID_H / 2].contains('@') || frame[GRID_H / 2].contains('#'));
    }

    #[test]
    fn ease_out_is_bounded_and_monotone_at_the_ends() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert!(ease_out(0.5) > 0.5); // ease-OUT front-loads the motion
    }

    #[test]
    fn cast_art_is_rectangular_enough_to_center() {
        for cat in [&QUELPAW[..], &QUELPAW_EAR[..], &QUELPAW_TAIL[..], &ECHO[..]] {
            assert!(cat.iter().all(|l| l.chars().count() == 12));
        }
        assert!(MOCHI.iter().all(|l| l.chars().count() == 11));
    }
}

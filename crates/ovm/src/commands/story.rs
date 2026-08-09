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
const MAGENTA: &str = "\x1b[35m";
const ORANGE: &str = "\x1b[38;5;215m";
const RUST: &str = "\x1b[38;5;130m";
const GREY: &str = "\x1b[38;5;245m";
/// Quelpaw's true coat — the original buddy was grey.
const SILVER: &str = "\x1b[38;5;250m";

// ---- the cast ---------------------------------------------------------------

const MOCHI: [&str; 3] = [r"   /\_/\   ", r"  ( ^.^ )  ", r"   > ^ <   "];
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
        for ch in text.chars() {
            print!("{ch}");
            let _ = out.flush();
            sleep(Duration::from_millis(pace_ms));
        }
        println!("{}", if color.is_empty() { "" } else { RESET });
    }

    fn blank(&self) {
        println!();
    }

    fn art(&self, lines: &[&str], color: &str) {
        let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
        let left = " ".repeat(self.width.saturating_sub(widest) / 2);
        for line in lines {
            println!("{left}{color}{line}{RESET}");
            self.beat(80);
        }
    }

    /// Draw the first frame, then rewrite the same lines in place.
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

    /// Chapter wait that can also launch a URL: `o` + enter opens it.
    fn wait_open(&self, url: &str, label: &str) {
        if self.fast {
            return;
        }
        let prompt = format!(
            "{}{DIM}· enter · o opens {label} ·{RESET}",
            " ".repeat(2.max(self.width.saturating_sub(40) / 2))
        );
        loop {
            print!("{prompt}");
            let _ = io::stdout().flush();
            let mut line = String::new();
            if io::stdin().read_line(&mut line).is_err() || line.is_empty() {
                return;
            }
            print!("\x1b[F\x1b[2K");
            let _ = io::stdout().flush();
            if matches!(line.trim().to_lowercase().as_str(), "o" | "open") {
                open_url(url);
                continue; // browser is opening; enter still advances the story
            }
            return;
        }
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
        self.fresh_page();
        self.blank();
        self.say(
            "Seventeen releases they rode along. Then 2.1.97 removed them.",
            "",
            12,
            300,
        );
        self.say(
            "The arrival got a changelog line. The goodbye got nothing.",
            "",
            12,
            0,
        );
        self.blank();
        self.say(
            "We kept 2.1.96. That is half the reason OVM exists:",
            "",
            10,
            0,
        );
        self.say(
            "every version, verified, one command away. The cat stays.",
            "",
            10,
            0,
        );
        self.wait();
    }

    fn chapter_mochi(&self) {
        self.fresh_page();
        self.blank();
        println!("{}", self.center(&format!("{MAGENTA}ii. mochi{RESET}"), 9));
        self.blank();
        self.art(&MOCHI, MAGENTA);
        self.blank();
        self.say(
            "Mochi came first, though — they wandered out of OpenClaw,",
            "",
            12,
            0,
        );
        self.say("long before any of this had a name.", "", 12, 0);
        self.blank();
        self.say(
            "When Codex grew a pet of its own, we asked for Mochi again —",
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
            "back one more time — to greet every ovm and cxy user.",
            "",
            10,
            0,
        );
        self.blank();
        self.say(
            "Find more Mochi — emoji plates included — at mochiexists.com",
            MAGENTA,
            12,
            300,
        );
        self.wait_open("https://mochiexists.com", "mochiexists.com");
    }

    fn chapter_echo(&self) {
        self.fresh_page();
        self.blank();
        println!("{}", self.center(&format!("{ORANGE}iii. echo{RESET}"), 9));
        self.blank();
        self.art(&ECHO, ORANGE);
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
        self.say("Not a copy. A reflection: Echo.", ORANGE, 12, 400);
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

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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

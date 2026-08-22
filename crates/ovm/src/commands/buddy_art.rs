//! The buddy creatures, recovered from Claude Code 2.1.96.
//!
//! `/buddy` wrote only `{name, personality, hatchedAt}` into the config — the
//! creature itself was drawn at render time, and that code left with the
//! feature in 2.1.97. But the species survives inside the personality line
//! ("A uncommon octopus of few words."), and the frames themselves are still
//! in the 2.1.96 binary OVM keeps installed. So the card can show the animal
//! the reader actually hatched instead of a stand-in cat, without inventing
//! anything: the story's own rule.
//!
//! Only the terse personality line names its species. The chatty format the
//! earliest buddies got ("Debugging genius with the patience of...") does not,
//! and 2.1.96 derived those at render time from something it never wrote down
//! — so they fall back to the chapter's own cat rather than to a guess.
//!
//! The eye glyph is likewise unrecoverable (2.1.96 chose one per buddy), so
//! every creature here wears the same one. Hats are omitted for the same
//! reason.
//!
//! `chonk` is the art the story ships as `QUELPAW` — the cat that was removed
//! was a chonk.

static AXOLOTL: [&[&str]; 3] = [
    &[
        r"}~(______)~{",
        r"}~(● .. ●)~{",
        r"  ( .--. )  ",
        r"  (_/  \_)  ",
    ],
    &[
        r"~}(______){~",
        r"~}(● .. ●){~",
        r"  ( .--. )  ",
        r"  (_/  \_)  ",
    ],
    &[
        r"}~(______)~{",
        r"}~(● .. ●)~{",
        r"  (  --  )  ",
        r"  ~_/  \_~  ",
    ],
];
static BLOB: [&[&str]; 3] = [
    &[
        r"   .----.   ",
        r"  ( ●  ● )  ",
        r"  (      )  ",
        r"   `----´   ",
    ],
    &[
        r"  .------.  ",
        r" (  ●  ●  ) ",
        r" (        ) ",
        r"  `------´  ",
    ],
    &[
        r"    .--.    ",
        r"   (●  ●)   ",
        r"   (    )   ",
        r"    `--´    ",
    ],
];
static CACTUS: [&[&str]; 3] = [
    &[
        r"            ",
        r" n  ____  n ",
        r" | |●  ●| | ",
        r" |_|    |_| ",
        r"   |    |   ",
    ],
    &[
        r"            ",
        r"    ____    ",
        r" n |●  ●| n ",
        r" |_|    |_| ",
        r"   |    |   ",
    ],
    &[
        r" n        n ",
        r" |  ____  | ",
        r" | |●  ●| | ",
        r" |_|    |_| ",
        r"   |    |   ",
    ],
];
static CAPYBARA: [&[&str]; 3] = [
    &[
        r"            ",
        r"  n______n  ",
        r" ( ●    ● ) ",
        r" (   oo   ) ",
        r"  `------´  ",
    ],
    &[
        r"            ",
        r"  n______n  ",
        r" ( ●    ● ) ",
        r" (   Oo   ) ",
        r"  `------´  ",
    ],
    &[
        r"    ~  ~    ",
        r"  u______n  ",
        r" ( ●    ● ) ",
        r" (   oo   ) ",
        r"  `------´  ",
    ],
];
static CAT: [&[&str]; 3] = [
    &[
        r"   /\_/\    ",
        r"  ( ●   ●)  ",
        r"  (  ω  )   ",
        r#"  (")_(")   "#,
    ],
    &[
        r"   /\_/\    ",
        r"  ( ●   ●)  ",
        r"  (  ω  )   ",
        r#"  (")_(")~  "#,
    ],
    &[
        r"   /\-/\    ",
        r"  ( ●   ●)  ",
        r"  (  ω  )   ",
        r#"  (")_(")   "#,
    ],
];
static CHONK: [&[&str]; 3] = [
    &[
        r"  /\    /\  ",
        r" ( ●    ● ) ",
        r" (   ..   ) ",
        r"  `------´  ",
    ],
    &[
        r"  /\    /|  ",
        r" ( ●    ● ) ",
        r" (   ..   ) ",
        r"  `------´  ",
    ],
    &[
        r"  /\    /\  ",
        r" ( ●    ● ) ",
        r" (   ..   ) ",
        r"  `------´~ ",
    ],
];
static DRAGON: [&[&str]; 3] = [
    &[
        r"            ",
        r"  /^\  /^\  ",
        r" <  ●  ●  > ",
        r" (   ~~   ) ",
        r"  `-vvvv-´  ",
    ],
    &[
        r"            ",
        r"  /^\  /^\  ",
        r" <  ●  ●  > ",
        r" (        ) ",
        r"  `-vvvv-´  ",
    ],
    &[
        r"   ~    ~   ",
        r"  /^\  /^\  ",
        r" <  ●  ●  > ",
        r" (   ~~   ) ",
        r"  `-vvvv-´  ",
    ],
];
static DUCK: [&[&str]; 3] = [
    &[
        r"    __      ",
        r"  <(● )___  ",
        r"   (  ._>   ",
        r"    `--´    ",
    ],
    &[
        r"    __      ",
        r"  <(● )___  ",
        r"   (  ._>   ",
        r"    `--´~   ",
    ],
    &[
        r"    __      ",
        r"  <(● )___  ",
        r"   (  .__>  ",
        r"    `--´    ",
    ],
];
static GHOST: [&[&str]; 3] = [
    &[
        r"            ",
        r"   .----.   ",
        r"  / ●  ● \  ",
        r"  |      |  ",
        r"  ~`~``~`~  ",
    ],
    &[
        r"            ",
        r"   .----.   ",
        r"  / ●  ● \  ",
        r"  |      |  ",
        r"  `~`~~`~`  ",
    ],
    &[
        r"    ~  ~    ",
        r"   .----.   ",
        r"  / ●  ● \  ",
        r"  |      |  ",
        r"  ~~`~~`~~  ",
    ],
];
static GOOSE: [&[&str]; 3] = [
    &[
        r"     (●>    ",
        r"     ||     ",
        r"   _(__)_   ",
        r"    ^^^^    ",
    ],
    &[
        r"    (●>     ",
        r"     ||     ",
        r"   _(__)_   ",
        r"    ^^^^    ",
    ],
    &[
        r"     (●>>   ",
        r"     ||     ",
        r"   _(__)_   ",
        r"    ^^^^    ",
    ],
];
static MUSHROOM: [&[&str]; 3] = [
    &[
        r"            ",
        r" .-o-OO-o-. ",
        r"(__________)",
        r"   |●  ●|   ",
        r"   |____|   ",
    ],
    &[
        r"            ",
        r" .-O-oo-O-. ",
        r"(__________)",
        r"   |●  ●|   ",
        r"   |____|   ",
    ],
    &[
        r"   . o  .   ",
        r" .-o-OO-o-. ",
        r"(__________)",
        r"   |●  ●|   ",
        r"   |____|   ",
    ],
];
static OCTOPUS: [&[&str]; 3] = [
    &[
        r"            ",
        r"   .----.   ",
        r"  ( ●  ● )  ",
        r"  (______)  ",
        r"  /\/\/\/\  ",
    ],
    &[
        r"            ",
        r"   .----.   ",
        r"  ( ●  ● )  ",
        r"  (______)  ",
        r"  \/\/\/\/  ",
    ],
    &[
        r"     o      ",
        r"   .----.   ",
        r"  ( ●  ● )  ",
        r"  (______)  ",
        r"  /\/\/\/\  ",
    ],
];
static OWL: [&[&str]; 3] = [
    &[
        r"   /\  /\   ",
        r"  ((●)(●))  ",
        r"  (  ><  )  ",
        r"   `----´   ",
    ],
    &[
        r"   /\  /\   ",
        r"  ((●)(●))  ",
        r"  (  ><  )  ",
        r"   .----.   ",
    ],
    &[
        r"   /\  /\   ",
        r"  ((●)(-))  ",
        r"  (  ><  )  ",
        r"   `----´   ",
    ],
];
static PENGUIN: [&[&str]; 3] = [
    &[
        r"            ",
        r"  .---.     ",
        r"  (●>●)     ",
        r" /(   )\    ",
        r"  `---´     ",
    ],
    &[
        r"            ",
        r"  .---.     ",
        r"  (●>●)     ",
        r" |(   )|    ",
        r"  `---´     ",
    ],
    &[
        r"  .---.     ",
        r"  (●>●)     ",
        r" /(   )\    ",
        r"  `---´     ",
        r"   ~ ~      ",
    ],
];
static RABBIT: [&[&str]; 3] = [
    &[
        r"   (\__/)   ",
        r"  ( ●  ● )  ",
        r" =(  ..  )= ",
        r#"  (")__(")  "#,
    ],
    &[
        r"   (|__/)   ",
        r"  ( ●  ● )  ",
        r" =(  ..  )= ",
        r#"  (")__(")  "#,
    ],
    &[
        r"   (\__/)   ",
        r"  ( ●  ● )  ",
        r" =( .  . )= ",
        r#"  (")__(")  "#,
    ],
];
static ROBOT: [&[&str]; 3] = [
    &[
        r"            ",
        r"   .[||].   ",
        r"  [ ●  ● ]  ",
        r"  [ ==== ]  ",
        r"  `------´  ",
    ],
    &[
        r"            ",
        r"   .[||].   ",
        r"  [ ●  ● ]  ",
        r"  [ -==- ]  ",
        r"  `------´  ",
    ],
    &[
        r"     *      ",
        r"   .[||].   ",
        r"  [ ●  ● ]  ",
        r"  [ ==== ]  ",
        r"  `------´  ",
    ],
];
static SNAIL: [&[&str]; 3] = [
    &[
        r" ●    .--.  ",
        r"  \  ( @ )  ",
        r"   \_`--´   ",
        r"  ~~~~~~~   ",
    ],
    &[
        r"  ●   .--.  ",
        r"  |  ( @ )  ",
        r"   \_`--´   ",
        r"  ~~~~~~~   ",
    ],
    &[
        r" ●    .--.  ",
        r"  \  ( @  ) ",
        r"   \_`--´   ",
        r"   ~~~~~~   ",
    ],
];
static TURTLE: [&[&str]; 3] = [
    &[
        r"   _,--._   ",
        r"  ( ●  ● )  ",
        r" /[______]\ ",
        r"  ``    ``  ",
    ],
    &[
        r"   _,--._   ",
        r"  ( ●  ● )  ",
        r" /[______]\ ",
        r"   ``  ``   ",
    ],
    &[
        r"   _,--._   ",
        r"  ( ●  ● )  ",
        r" /[======]\ ",
        r"  ``    ``  ",
    ],
];

/// The five tiers, in order — the star count is the position, which is how
/// 2.1.96 drew them (`★ COMMON`, `★★ UNCOMMON`).
pub(super) const RARITIES: [&str; 5] = ["common", "uncommon", "rare", "epic", "legendary"];

const SPECIES: [&str; 18] = [
    "axolotl", "blob", "cactus", "capybara", "cat", "chonk", "dragon", "duck", "ghost", "goose",
    "mushroom", "octopus", "owl", "penguin", "rabbit", "robot", "snail", "turtle",
];

/// Read the rarity and species back out of a personality line.
///
/// Word-matching rather than a strict "A {rarity} {species} of few words."
/// parse: a word that is one of the eighteen species is not there by accident,
/// and the older chatty lines simply match nothing.
pub(super) fn read_personality(personality: &str) -> (Option<&'static str>, Option<&'static str>) {
    let lower = personality.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|word| !word.is_empty())
        .collect();
    let rarity = RARITIES.iter().copied().find(|tier| words.contains(tier));
    let species = SPECIES.iter().copied().find(|name| words.contains(name));
    (rarity, species)
}

/// `★★` for uncommon, and so on.
pub(super) fn stars(rarity: &str) -> String {
    let tier = RARITIES
        .iter()
        .position(|name| *name == rarity)
        .unwrap_or(0);
    "★".repeat(tier + 1)
}

/// The frames for a species name lifted out of a personality line, or `None`
/// when the word is not one of the eighteen 2.1.96 shipped.
pub(super) fn frames_for(species: &str) -> Option<&'static [&'static [&'static str]]> {
    Some(match species {
        "axolotl" => &AXOLOTL,
        "blob" => &BLOB,
        "cactus" => &CACTUS,
        "capybara" => &CAPYBARA,
        "cat" => &CAT,
        "chonk" => &CHONK,
        "dragon" => &DRAGON,
        "duck" => &DUCK,
        "ghost" => &GHOST,
        "goose" => &GOOSE,
        "mushroom" => &MUSHROOM,
        "octopus" => &OCTOPUS,
        "owl" => &OWL,
        "penguin" => &PENGUIN,
        "rabbit" => &RABBIT,
        "robot" => &ROBOT,
        "snail" => &SNAIL,
        "turtle" => &TURTLE,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_species_and_rarity_a_real_config_carries() {
        let (rarity, species) = read_personality("A uncommon octopus of few words.");
        assert_eq!(rarity, Some("uncommon"));
        assert_eq!(species, Some("octopus"));
    }

    /// The card indexes frame 0 and every row of it, so an empty entry is a
    /// panic in the story rather than a missing drawing. An extraction bug
    /// shipped exactly that for `cat` and `rabbit`, and a test that only
    /// asserted `is_some()` waved it through.
    #[test]
    fn every_species_has_drawable_frames() {
        for name in SPECIES {
            let frames = frames_for(name).unwrap_or_else(|| panic!("{name} has no frames"));
            assert!(!frames.is_empty(), "{name} has zero frames");
            for (index, frame) in frames.iter().enumerate() {
                assert!(!frame.is_empty(), "{name} frame {index} has no rows");
                assert!(
                    frame.iter().any(|row| !row.trim().is_empty()),
                    "{name} frame {index} is entirely blank"
                );
            }
        }
    }

    #[test]
    fn an_unparseable_line_yields_nothing_rather_than_a_guess() {
        let (rarity, species) = read_personality(
            "Debugging genius with the patience of a caffeinated squirrel—finds your bugs.",
        );
        assert_eq!(rarity, None);
        assert_eq!(species, None);
    }

    #[test]
    fn stars_track_the_tier() {
        assert_eq!(stars("common"), "★");
        assert_eq!(stars("uncommon"), "★★");
        assert_eq!(stars("legendary"), "★★★★★");
    }
}

//! The game's `@key` texts. Game data names things by a key (`@RepScope_Courier_Name`) that the client looks up in its
//! `global.ini`; the admin site does the same with the English file that `scripts/game-files.sh` extracts, so a name reads
//! as the player sees it. Without the file a key is shown as it is, without the `@`.

use std::{collections::HashMap, sync::OnceLock};

include!(concat!(env!("OUT_DIR"), "/localization_texts.rs"));

/// What a key starts with.
const KEY_MARK: char = '@';
/// The byte order mark the file starts with.
const BYTE_ORDER_MARK: char = '\u{feff}';
/// The separator of a line's key and text.
const SEPARATOR: char = '=';
/// The placeholders the game data uses for "no text".
const NO_TEXT: [&str; 2] = ["LOC_PLACEHOLDER", "blank_space"];

/// The texts by lowercase key.
pub struct Texts(HashMap<String, &'static str>);

impl Texts {
    /// Reads `key=text` lines; a line with no separator is skipped.
    pub fn parse(source: &'static str) -> Self {
        let source = source.strip_prefix(BYTE_ORDER_MARK).unwrap_or(source);
        let mut texts = HashMap::new();
        for line in source.lines() {
            if let Some((key, text)) = line.split_once(SEPARATOR) {
                texts.insert(key.trim().to_ascii_lowercase(), text.trim_end());
            }
        }
        Self(texts)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The text of a name: a `@key` is looked up (the key itself, without the `@`, when the file lacks it), a placeholder is
    /// empty, and anything else is the text already.
    pub fn text(&self, name: &str) -> String {
        let Some(key) = name.strip_prefix(KEY_MARK) else {
            return if NO_TEXT.contains(&name) { String::new() } else { name.to_string() };
        };
        if NO_TEXT.contains(&key) {
            return String::new();
        }
        self.0.get(&key.to_ascii_lowercase()).map_or_else(|| key.to_string(), |text| (*text).to_string())
    }
}

/// The game's texts, read once.
pub fn texts() -> &'static Texts {
    static TEXTS_ONCE: OnceLock<Texts> = OnceLock::new();
    TEXTS_ONCE.get_or_init(|| Texts::parse(TEXTS))
}

/// `Texts::text` of the game's texts.
pub fn text(name: &str) -> String {
    texts().text(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\u{feff}RepScope_Courier_Name=Courier\r\nrepstanding_perk_bonus_15=+15% Payment Bonus\r\nnot a line\r\nA=b=c\r\n";

    #[test]
    fn a_key_is_looked_up_without_regard_to_case() {
        let texts = Texts::parse(SAMPLE);
        assert_eq!(texts.text("@RepScope_Courier_Name"), "Courier");
        assert_eq!(texts.text("@RepStanding_Perk_Bonus_15"), "+15% Payment Bonus");
        assert_eq!(texts.text("@a"), "b=c", "the text is what follows the first separator");
    }

    #[test]
    fn what_has_no_text_reads_plainly() {
        let texts = Texts::parse(SAMPLE);
        assert_eq!(texts.text("@Unknown_Key"), "Unknown_Key", "a missing key shows without the mark");
        assert_eq!(texts.text("@LOC_PLACEHOLDER"), "");
        assert_eq!(texts.text("blank_space"), "");
        assert_eq!(texts.text("Courier"), "Courier", "plain text is kept");
        assert!(Texts::parse("").is_empty());
    }
}

#[cfg(test)]
mod game_texts {
    /// With the game's texts built in (scripts/game-files.sh), a scope key reads as the client shows it.
    #[test]
    fn the_games_texts_name_a_reputation_scope() {
        if super::texts().is_empty() {
            return;
        }
        assert_eq!(super::text("@RepScope_Courier_Name"), "Courier");
        assert_eq!(super::text("@RepScope_Emergency_Name"), "Emergency Support");
    }
}

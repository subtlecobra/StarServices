//! How long each hangar class's loading platform cycle lasts, from the game data's `tables/platform_times.tsv`
//! (`tools/gamedata-build`, docs/DATA.md): the client's pad and gate doors move for a time of their own animation, so the
//! platform's state (and the lights that follow it) ends when the doors have. A class the table lacks keeps the fixed
//! times of `ships.rs`.

use std::{collections::HashMap, sync::OnceLock, time::Duration};

use crate::platform::OPENING_DELAY;

/// The path of the table in a game build, as `tools/gamedata-build` writes it and `sc-gamedata` serves it.
pub const PLATFORM_TIMES_PATH: &str = "tables/platform_times.tsv";

/// The columns of a row: the class, then in milliseconds the opening and closing sequence, pad door and gate door.
const COLUMNS: usize = 7;
const OPEN_SEQUENCE_COLUMN: usize = 1;
const CLOSE_SEQUENCE_COLUMN: usize = 2;
const PAD_OPEN_COLUMN: usize = 3;
const PAD_CLOSE_COLUMN: usize = 4;
const GATE_OPEN_COLUMN: usize = 5;
const GATE_CLOSE_COLUMN: usize = 6;
/// What the platform waits after its doors have finished closing, for the client's last frame and the ship's bind.
pub const CLOSE_MARGIN: Duration = Duration::from_millis(500);
/// What it waits after the doors have finished opening, on top of `OPENING_DELAY`, the pause before they start.
pub const OPEN_MARGIN: Duration = Duration::from_millis(300);

/// How long the platform takes each way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Times {
    pub open: Duration,
    pub close: Duration,
    /// How long the loading gate's door takes to open: the pad's door and the platform wait for it.
    pub gate_open: Duration,
    /// How long the pad's door takes to close: the gate's door waits for it.
    pub pad_close: Duration,
}

/// The platform times of each hangar class of a build.
#[derive(Debug, Default)]
pub struct PlatformTimes {
    classes: HashMap<String, Times>,
}

impl PlatformTimes {
    /// The table. A row whose doors have no time (the game data has no clip for them) is left out, so its class keeps
    /// the fixed times; a row that does not read is an error.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut classes = HashMap::new();
        for (index, line) in text.lines().enumerate().filter(|(_, line)| !line.is_empty()) {
            let fields: Vec<&str> = line.split('\t').collect();
            let number = |column: usize| -> Result<u64, String> {
                let field = fields.get(column).ok_or_else(|| format!("line {}: {} fields", index + 1, fields.len()))?;
                field.parse().map_err(|_| format!("line {}: {field:?} is not a number", index + 1))
            };
            if fields.len() != COLUMNS {
                return Err(format!("line {}: {} fields, not {COLUMNS}", index + 1, fields.len()));
            }
            let (pad_open, gate_open) = (number(PAD_OPEN_COLUMN)?, number(GATE_OPEN_COLUMN)?);
            let doors_open = pad_open.max(gate_open);
            let (pad_close, gate_close) = (number(PAD_CLOSE_COLUMN)?, number(GATE_CLOSE_COLUMN)?);
            let doors_close = pad_close.max(gate_close);
            if doors_open == 0 || doors_close == 0 {
                continue;
            }
            // The gate opens, then the pad's door follows: the cycle is the two doors one after the other, and lasts at
            // least as long as the lights and hologram sequence, which runs from the state's start (the lights go to their
            // idle setting when the state ends).
            let doors = OPENING_DELAY + Duration::from_millis(gate_open + pad_open) + OPEN_MARGIN;
            let sequence = |column: usize| number(column).map(Duration::from_millis);
            let times = Times {
                open: doors.max(sequence(OPEN_SEQUENCE_COLUMN)? + OPEN_MARGIN),
                // The pad's door closes, then the gate's: one after the other, as they open in the other order.
                close: Duration::from_millis(pad_close + gate_close).max(sequence(CLOSE_SEQUENCE_COLUMN)?) + CLOSE_MARGIN,
                gate_open: Duration::from_millis(gate_open),
                pad_close: Duration::from_millis(pad_close),
            };
            classes.insert(fields[0].to_string(), times);
        }
        Ok(Self { classes })
    }

    /// The times of a hangar class, if the table has them.
    pub fn of(&self, class: &str) -> Option<Times> {
        self.classes.get(class).copied()
    }

    pub fn classes(&self) -> usize {
        self.classes.len()
    }
}

static INSTALLED: OnceLock<PlatformTimes> = OnceLock::new();

/// Sets the table for the life of the process, once, at startup. `false` when it was already set.
pub fn install(times: PlatformTimes) -> bool {
    INSTALLED.set(times).is_ok()
}

/// The installed times of a hangar class: none before `install` (and in tests), so the fixed times stand.
pub fn installed_of(class: &str) -> Option<Times> {
    INSTALLED.get()?.of(class)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic rows: a small hangar's doors move 12.5 s, a large one's 15 s and its gate's 17 s, one class has no pad.
    const TABLE: &str = "Hangar_Small\t33000\t31667\t12500\t12500\t12500\t12500\n\
                         Hangar_Large\t39000\t37667\t15000\t15000\t17000\t17000\n\
                         Hangar_NoClip\t33000\t31667\t0\t0\t0\t0\n";

    #[test]
    fn a_class_takes_its_slowest_door_and_a_margin() {
        let table = PlatformTimes::parse(TABLE).unwrap();
        assert_eq!(table.classes(), 2);
        let small = table.of("Hangar_Small").unwrap();
        // The lights sequence (33 s / 31.667 s) outlasts the doors (12.5 s each, one after the other).
        assert_eq!(small.close, Duration::from_millis(32_167));
        assert_eq!(small.open, Duration::from_millis(33_300));
        assert_eq!(small.gate_open, Duration::from_millis(12_500));
        let large = table.of("Hangar_Large").unwrap();
        assert_eq!(large.close, Duration::from_millis(38_167));
        assert_eq!(large.gate_open, Duration::from_millis(17_000));
        assert!(large.open > small.open);
    }

    #[test]
    fn the_platform_rises_after_both_doors_when_they_outlast_the_lights() {
        let table = PlatformTimes::parse("Hangar_Slow\t10000\t10000\t20000\t20000\t20000\t20000\n").unwrap();
        let slow = table.of("Hangar_Slow").unwrap();
        assert_eq!(slow.open, OPENING_DELAY + Duration::from_millis(40_000) + OPEN_MARGIN);
        assert_eq!(slow.close, Duration::from_millis(40_000) + CLOSE_MARGIN);
        assert_eq!(slow.pad_close, Duration::from_millis(20_000));
    }

    #[test]
    fn a_class_without_door_times_or_not_in_the_table_has_none() {
        let table = PlatformTimes::parse(TABLE).unwrap();
        assert_eq!(table.of("Hangar_NoClip"), None);
        assert_eq!(table.of("Lobby"), None);
        assert_eq!(installed_of("Hangar_Small"), None);
    }

    #[test]
    fn a_broken_row_is_an_error_and_an_empty_table_is_not() {
        assert!(PlatformTimes::parse("Hangar_Small\t1\t2").unwrap_err().contains("line 1"));
        assert!(PlatformTimes::parse("Hangar_Small\ta\tb\tc\td\te\tf").unwrap_err().contains("not a number"));
        assert_eq!(PlatformTimes::parse("").unwrap().classes(), 0);
    }
}

//! The game files tests read from `testdata/`: the tables of a game build and the rest stop hangar's collision, laid out
//! as a build directory of `tools/gamedata-build`. They come from the game, so they are not in the repository:
//! `scripts/game-files.sh` generates them from your copy of the game. Read at run time rather than compiled in, so that a
//! test without them stops with a pointer to that script, and so that nothing but tests ever needs them.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{LazyLock, Mutex},
};

/// `testdata/` of this crate.
pub fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

/// A file of `testdata/`, read once for the life of the process. Panics with what to run when it is not there.
pub fn bytes(path: &str) -> &'static [u8] {
    static READ: LazyLock<Mutex<HashMap<String, &'static [u8]>>> = LazyLock::new(Default::default);
    if let Some(data) = READ.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(path) {
        return data;
    }
    // Read without the lock, so that a missing file fails each test that wants it with the same message.
    let full = dir().join(path);
    let data = std::fs::read(&full).unwrap_or_else(|error| missing(&full, &error));
    let mut read = READ.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *read.entry(path.to_string()).or_insert_with(|| Box::leak(data.into_boxed_slice()))
}

/// A text file of `testdata/` by its path, read on first use: `TABLE.lines()` for `const TABLE: Text = Text(path)`.
pub struct Text(pub &'static str);

impl std::ops::Deref for Text {
    type Target = str;

    fn deref(&self) -> &str {
        text(self.0)
    }
}

/// As `bytes`, for a text file.
pub fn text(path: &str) -> &'static str {
    std::str::from_utf8(bytes(path)).unwrap_or_else(|error| panic!("testdata/{path} is not UTF-8: {error}"))
}

fn missing(path: &std::path::Path, error: &std::io::Error) -> ! {
    panic!(
        "{}: {error}. The test data comes from the game and is not in the repository: generate it from your copy of the \
         game with scripts/game-files.sh (see README, \"Game files\")",
        path.display()
    )
}

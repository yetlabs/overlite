//! `OVERLITE_RECORD_STATEMENTS=<file>`: every statement a client sends, a simple Query's
//! (each of a batch) and a Parse's, is appended to the file as a line of hex, as the
//! AST corpus keeps them (tests/fixtures/ast-corpus). `make ast-corpus` runs the wire
//! suite with it and adds what is new to the corpus, so the differential test grows
//! with the suite.

use overlite_core::sync::MutexExt;
use std::fs::File;
use std::io::Write;
use std::sync::{LazyLock, Mutex};

static FILE: LazyLock<Option<Mutex<File>>> = LazyLock::new(|| {
    let path = std::env::var_os("OVERLITE_RECORD_STATEMENTS")?;
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => Some(Mutex::new(f)),
        Err(e) => {
            log::warn!("OVERLITE_RECORD_STATEMENTS: cannot open {}: {e}", path.display());
            None
        }
    }
});

/// Appends sql to the record, when one is kept.
pub(crate) fn statement(sql: &str) {
    if let Some(f) = FILE.as_ref() {
        let line = hex::encode(sql.as_bytes()) + "\n";
        // One write per line: servers of a suite append to one file, and an
        // O_APPEND write of a line lands whole.
        if let Err(e) = f.locked().write_all(line.as_bytes()) {
            log::warn!("OVERLITE_RECORD_STATEMENTS: {e}");
        }
    }
}

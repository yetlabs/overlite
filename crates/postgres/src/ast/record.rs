//! `OVERLITE_RECORD_STATEMENTS=<file>`: every statement a client sends, a simple Query's
//! (each of a batch) and a Parse's, is appended to the file as a line of hex, as the
//! AST corpus keeps them (tests/fixtures/ast-corpus). `make ast-corpus` runs the wire
//! suite with it and adds what is new to the corpus, so the differential test grows
//! with the suite.
//!
//! `OVERLITE_RECORD_TEXT_PASSES=<file>`: every statement `rewrite` hands to the text
//! passes (`rewrite_full`), the same way, so what still reaches them in a running
//! server can be counted by node kind (`count_what_reached_the_text_passes`, in
//! `diff.rs`), not only in the corpus.

use overlite_core::sync::MutexExt;
use std::fs::File;
use std::io::Write;
use std::sync::{LazyLock, Mutex};

static FILE: LazyLock<Option<Mutex<File>>> = LazyLock::new(|| open("OVERLITE_RECORD_STATEMENTS"));

static TEXT_PASSES: LazyLock<Option<Mutex<File>>> = LazyLock::new(|| open("OVERLITE_RECORD_TEXT_PASSES"));

fn open(var: &str) -> Option<Mutex<File>> {
    let path = std::env::var_os(var)?;
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => Some(Mutex::new(f)),
        Err(e) => {
            log::warn!("{var}: cannot open {}: {e}", path.display());
            None
        }
    }
}

fn append(file: Option<&Mutex<File>>, sql: &str) {
    if let Some(f) = file {
        let line = hex::encode(sql.as_bytes()) + "\n";
        // One write per line: servers of a suite append to one file, and an
        // O_APPEND write of a line lands whole.
        if let Err(e) = f.locked().write_all(line.as_bytes()) {
            log::warn!("recording statements: {e}");
        }
    }
}

/// Appends sql to the record, when one is kept.
pub(crate) fn statement(sql: &str) {
    append(FILE.as_ref(), sql);
}

/// Appends sql to the record of what reached the text passes, when one is kept.
pub(crate) fn text_passes(sql: &str) {
    append(TEXT_PASSES.as_ref(), sql);
}

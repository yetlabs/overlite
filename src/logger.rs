//! The `log` backend: one line on stderr, stamped with the local time.

use log::{Level, LevelFilter, Log, Metadata, Record};
use overlite_postgres::UNSTAMPED_LOG_TARGET;
use std::io::Write;

struct StderrLogger;

static LOGGER: StderrLogger = StderrLogger;

impl Log for StderrLogger {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        let args = record.args();
        // stderr gone is nothing a log line can report.
        let _ = if record.target() == UNSTAMPED_LOG_TARGET {
            writeln!(std::io::stderr(), "{args}")
        } else {
            let now = chrono::Local::now().format("%Y/%m/%d %H:%M:%S");
            writeln!(std::io::stderr(), "{now} {args}")
        };
    }

    fn flush(&self) {}
}

/// The level OVERLITE_DEBUG asks for: unset or empty leaves failed queries out,
/// any value logs them, and `all` adds every executed query.
fn level(debug: Option<&str>) -> LevelFilter {
    match debug {
        None | Some("") => Level::Info.to_level_filter(),
        Some("all") => Level::Trace.to_level_filter(),
        Some(_) => Level::Debug.to_level_filter(),
    }
}

/// Installs the logger. A second call would be the only way to fail.
pub fn init() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level(std::env::var("OVERLITE_DEBUG").ok().as_deref()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlite_debug_sets_the_level() {
        assert_eq!(level(None), LevelFilter::Info);
        assert_eq!(level(Some("")), LevelFilter::Info);
        assert_eq!(level(Some("1")), LevelFilter::Debug);
        assert_eq!(level(Some("all")), LevelFilter::Trace);
    }
}

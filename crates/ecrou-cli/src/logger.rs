//! stderr logging sink: `Warn`/`Error` records always, `Debug`
//! diagnostics only with -v.

use log::{Level, LevelFilter, Log, Metadata, Record};

struct CliLogger {
    max: LevelFilter,
}

impl Log for CliLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        if metadata.level() > self.max {
            return false;
        }
        match metadata.level() {
            Level::Warn | Level::Error => true,
            // Dependencies (hyper, rustls, …) share the log facade and are
            // far noisier than we are; -v follows only our own diagnostics.
            _ => metadata.target().starts_with("ecrou"),
        }
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        match record.level() {
            Level::Error => eprintln!("Error: {}", record.args()),
            Level::Warn => eprintln!("Warning: {}", record.args()),
            _ => eprintln!("{}", record.args()),
        }
    }

    fn flush(&self) {}
}

/// Install the CLI logger: warnings and errors always, `ecrou` debug
/// diagnostics only when `verbose` (-v).
pub fn init(verbose: bool) {
    let max = if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Warn
    };
    log::set_boxed_logger(Box::new(CliLogger { max })).ok();
    log::set_max_level(max);
}

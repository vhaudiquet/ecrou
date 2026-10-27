//! Terminal rendering of library progress events (indicatif-backed).
//!
//! This is CLI-owned UI: the library only emits [`Progress`] events,
//! and everything drawn on the terminal lives here.  indicatif hides
//! all output when stderr is not a terminal.

use ecrou::progress::{Progress, Unit};
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::Mutex;

/// Renders sequential library operations as bars and spinners on stderr.
pub struct TermProgress {
    bar: Mutex<Option<ProgressBar>>,
}

impl TermProgress {
    pub fn new() -> Self {
        TermProgress {
            bar: Mutex::new(None),
        }
    }
}

impl Default for TermProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TermProgress {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Progress for TermProgress {
    fn begin(&self, label: &str, unit: Unit, total: Option<u64>) {
        let mut slot = self.bar.lock().unwrap();
        // The library's `begin` implicitly ends the previous operation.
        if let Some(prev) = slot.take() {
            prev.finish_and_clear();
        }

        let bar = match (unit, total) {
            (Unit::Bytes, Some(total)) => {
                let pb = ProgressBar::new(total);
                pb.set_style(
                    ProgressStyle::default_bar()
                        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {msg} {bytes}/{total_bytes} ({eta})")
                        .unwrap()
                        .progress_chars("#>-"),
                );
                pb.set_message(label.to_string());
                pb
            }
            (Unit::Bytes, None) => {
                let pb = ProgressBar::new_spinner();
                pb.set_style(
                    ProgressStyle::default_spinner()
                        .template("{spinner:.green} {msg} ({bytes})")
                        .unwrap(),
                );
                pb.set_message(label.to_string());
                pb
            }
            (Unit::Files, _) => {
                let pb = ProgressBar::new_spinner();
                pb.set_style(
                    ProgressStyle::default_spinner()
                        .template("{spinner:.green} {msg} ({pos} files)")
                        .unwrap(),
                );
                pb.set_message(label.to_string());
                pb
            }
        };
        *slot = Some(bar);
    }

    fn advance(&self, n: u64) {
        if let Some(bar) = &*self.bar.lock().unwrap() {
            bar.inc(n);
        }
    }

    fn finish(&self) {
        if let Some(bar) = self.bar.lock().unwrap().take() {
            bar.finish_and_clear();
        }
    }
}

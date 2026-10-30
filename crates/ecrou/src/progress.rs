//! Progress reporting for long-running operations.
//!
//! The library never touches the terminal: downloads and image builds
//! report their progress through the [`Progress`] sink supplied by the
//! caller.  Consumers choose what to render —
//! progress bars, spinners, structured events — or pass [`NoProgress`]
//! for silence.

/// What a progress operation counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Bytes transferred (downloads).
    Bytes,
    /// Files processed (archive merges, the image build).
    Files,
}

/// Sink for progress of long-running library operations.
///
/// Operations are sequential: `begin` starts the next operation (any
/// previous one is implicitly over), `advance` counts completed units,
/// `finish` marks its end.  `message` reports uncounted status outside
/// that protocol — a wait, a decision, a retry — when there is nothing
/// to count.  Sinks must be callable from any thread.
pub trait Progress: Send + Sync {
    /// An operation labelled `label` starts, counting `unit`s; `total`
    /// is the unit count when known upfront.
    fn begin(&self, label: &str, unit: Unit, total: Option<u64>);

    /// `n` more units of the current operation completed.
    fn advance(&self, n: u64);

    /// The current operation ended.
    fn finish(&self);

    /// A status line outside the counted-operation protocol. The
    /// default discards it; rendering sinks surface it as a spinner
    /// message, starting one if no operation is active.
    fn message(&self, _text: &str) {}
}

/// A [`Progress`] sink that discards every event.
pub struct NoProgress;

impl Progress for NoProgress {
    fn begin(&self, _: &str, _: Unit, _: Option<u64>) {}
    fn advance(&self, _: u64) {}
    fn finish(&self) {}
}

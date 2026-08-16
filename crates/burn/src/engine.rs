// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The engine adapter contract.
//!
//! Every engine (the fake one, xorriso, cdrdao) implements this. The trait
//! exists to keep one rule enforceable across all of them: an engine receives
//! a validated [`BurnPlan`] and converts it into its own fixed argument array.
//! No method takes command-line text, and no plan field carries any, so there
//! is no path by which a filename or a user string becomes an argument.
//!
//! Progress arrives through an [`EventSink`] rather than a return value,
//! because a write takes minutes and the operator needs to see it happen.

use async_trait::async_trait;

use crate::plan::{
    BlankReport, BlankRequest, BurnPlan, DriveCapabilities, DriveRef, MediumInfo, PreflightReport,
    VerifyReport, WriteReport,
};

/// Something an engine reports while working.
#[derive(Debug, Clone, PartialEq)]
pub struct BurnEvent {
    /// Which stage produced it.
    pub stage: &'static str,
    /// Stable machine-readable code. Prose may change; this may not.
    pub code: &'static str,
    /// Progress from 0 to 1, when the stage can report it.
    pub progress: Option<f32>,
    /// Human-readable detail.
    pub message: String,
}

impl BurnEvent {
    /// An event with no progress figure.
    #[must_use]
    pub fn new(stage: &'static str, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            stage,
            code,
            progress: None,
            message: message.into(),
        }
    }

    /// An event carrying progress.
    #[must_use]
    pub fn progress(
        stage: &'static str,
        code: &'static str,
        progress: f32,
        message: impl Into<String>,
    ) -> Self {
        Self {
            stage,
            code,
            progress: Some(progress.clamp(0.0, 1.0)),
            message: message.into(),
        }
    }
}

/// Where an engine sends progress.
///
/// Deliberately infallible. A failure to record progress must never abort a
/// write in flight: the disc is already being consumed, and stopping halfway
/// destroys it for no gain. A worker that cannot reach the server records
/// locally and reconciles afterwards.
pub trait EventSink: Send + Sync {
    /// Record one event.
    fn emit(&self, event: BurnEvent);
}

/// An event sink that discards everything, for callers that do not care.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: BurnEvent) {}
}

/// An event sink that collects into a vector, for tests and diagnostics.
#[derive(Debug, Default)]
pub struct CollectingSink {
    events: std::sync::Mutex<Vec<BurnEvent>>,
}

impl CollectingSink {
    /// A new empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything recorded so far.
    #[must_use]
    pub fn events(&self) -> Vec<BurnEvent> {
        self.events
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl EventSink for CollectingSink {
    fn emit(&self, event: BurnEvent) {
        // A poisoned lock drops the event rather than panicking: losing a
        // progress line is not worth aborting a burn over.
        if let Ok(mut guard) = self.events.lock() {
            guard.push(event);
        }
    }
}

/// Why an engine operation failed.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The drive could not be reached.
    #[error("drive {alias} is unavailable")]
    DriveUnavailable {
        /// The device alias.
        alias: String,
    },

    /// A write was attempted without a passing preflight.
    ///
    /// Refused by the engine itself, not only by the caller. A burn consumes
    /// physical media, so the last thing that can stop a mistake should also
    /// try to.
    #[error("refusing to write: preflight did not pass")]
    PreflightNotPassed,

    /// The medium is unsuitable.
    #[error("medium is unsuitable: {reason}")]
    UnsuitableMedium {
        /// What is wrong.
        reason: String,
    },

    /// The write failed partway.
    #[error("write failed after {bytes_written} bytes: {reason}")]
    WriteFailed {
        /// How far it got.
        bytes_written: u64,
        /// What went wrong.
        reason: String,
    },

    /// The operation was cancelled.
    #[error("operation cancelled")]
    Cancelled,

    /// An erase was requested without explicit confirmation.
    #[error("refusing to erase without explicit confirmation")]
    DestructiveNotConfirmed,

    /// Underlying I/O failed.
    #[error("{operation} failed")]
    Io {
        /// What was attempted.
        operation: &'static str,
        /// Cause.
        #[source]
        source: std::io::Error,
    },
}

/// A burn engine adapter.
#[async_trait]
pub trait BurnEngine: Send + Sync {
    /// The engine's name, recorded on every attempt.
    fn name(&self) -> &'static str;

    /// The engine's version, recorded so an old result can be explained.
    fn version(&self) -> String;

    /// What the drive reports it can do.
    ///
    /// # Errors
    ///
    /// [`EngineError::DriveUnavailable`] if the drive cannot be reached.
    async fn probe_drive(&self, drive: &DriveRef) -> Result<DriveCapabilities, EngineError>;

    /// What is in the drive.
    ///
    /// # Errors
    ///
    /// [`EngineError::DriveUnavailable`] if the drive cannot be reached.
    async fn inspect_medium(&self, drive: &DriveRef) -> Result<MediumInfo, EngineError>;

    /// Check a plan against the drive and medium without writing.
    ///
    /// Returns a report rather than an error for a plan that cannot proceed:
    /// "this disc is too small" is information the operator needs, not an
    /// exceptional condition.
    ///
    /// # Errors
    ///
    /// [`EngineError::DriveUnavailable`] if the drive cannot be inspected.
    async fn preflight(&self, plan: &BurnPlan) -> Result<PreflightReport, EngineError>;

    /// Write the plan to the medium.
    ///
    /// # Errors
    ///
    /// [`EngineError::PreflightNotPassed`] if preflight has not passed,
    /// [`EngineError::WriteFailed`] if the write fails, or
    /// [`EngineError::Cancelled`].
    async fn write(
        &self,
        plan: &BurnPlan,
        sink: &dyn EventSink,
    ) -> Result<WriteReport, EngineError>;

    /// Read the medium back and compare it to the plan.
    ///
    /// Separate from [`BurnEngine::write`] on purpose: a successful write is
    /// not a verified disc, and keeping them apart makes that impossible to
    /// blur.
    ///
    /// # Errors
    ///
    /// [`EngineError::DriveUnavailable`] or [`EngineError::Cancelled`].
    async fn verify(
        &self,
        plan: &BurnPlan,
        sink: &dyn EventSink,
    ) -> Result<VerifyReport, EngineError>;

    /// Erase a rewritable medium.
    ///
    /// # Errors
    ///
    /// [`EngineError::DestructiveNotConfirmed`] unless the request confirms
    /// the data loss is intended.
    async fn blank(
        &self,
        request: &BlankRequest,
        sink: &dyn EventSink,
    ) -> Result<BlankReport, EngineError>;

    /// Open the tray.
    ///
    /// # Errors
    ///
    /// [`EngineError::DriveUnavailable`] if the drive cannot be reached.
    async fn eject(&self, drive: &DriveRef) -> Result<(), EngineError>;
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_clamped_to_a_fraction() {
        // An engine parsing its own output can produce nonsense; the UI should
        // not have to defend against it.
        assert_eq!(
            BurnEvent::progress("writing", "PROGRESS", 5.0, "x").progress,
            Some(1.0)
        );
        assert_eq!(
            BurnEvent::progress("writing", "PROGRESS", -1.0, "x").progress,
            Some(0.0)
        );
    }

    #[test]
    fn a_collecting_sink_preserves_order() {
        let sink = CollectingSink::new();
        sink.emit(BurnEvent::new("a", "A", "first"));
        sink.emit(BurnEvent::new("b", "B", "second"));

        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].message, "first");
        assert_eq!(events[1].message, "second");
    }

    #[test]
    fn the_null_sink_accepts_everything() {
        NullSink.emit(BurnEvent::new("writing", "PROGRESS", "ignored"));
    }
}

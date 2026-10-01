// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! One engine per drive that can write both shapes of disc.
//!
//! xorriso writes prepared images and refuses a table of contents; cdrdao
//! writes a table of contents and is the wrong tool for an image. A worker
//! runs one engine, so a worker with either alone would claim jobs of the
//! other shape and fail them at preflight, because job claims do not yet take
//! a worker's engines into account. This engine holds one of each and gives
//! every plan to the one its write mode needs, so a single drive serves both.
//!
//! Questions about the drive rather than about a plan, probing, inspecting the
//! medium, ejecting, go to the data engine. xorriso reports real MMC profiles
//! for every family it can write, CD, DVD and Blu-ray, where cdrdao knows only
//! CDs; the table-of-contents engine still inspects the medium itself during
//! its own preflight. The drive is probed by both, so each engine's view of it
//! is on record.
//!
//! The engine that did the work names itself on the write report, so an
//! attempt records `xorriso` or `cdrdao`, never this wrapper.

use async_trait::async_trait;

use crate::engine::{BurnEngine, EngineError, EventSink};
use crate::plan::{
    BlankReport, BlankRequest, BurnPlan, DriveCapabilities, DriveRef, MediumInfo, PreflightReport,
    VerifyReport, WriteMode, WriteReport,
};

/// An engine that hands each plan to the engine its shape needs.
#[derive(Debug, Clone)]
pub struct CombinedEngine<D, T> {
    data: D,
    toc: T,
}

impl<D: BurnEngine, T: BurnEngine> CombinedEngine<D, T> {
    /// Combine an engine for prepared images with one for tables of contents.
    #[must_use]
    pub const fn new(data: D, toc: T) -> Self {
        Self { data, toc }
    }

    /// The engine that writes a disc of this shape.
    fn for_mode(&self, mode: WriteMode) -> &dyn BurnEngine {
        match mode {
            WriteMode::DataDiscAtOnce | WriteMode::DataTrackAtOnce => &self.data,
            WriteMode::TocDiscAtOnce => &self.toc,
        }
    }
}

#[async_trait]
impl<D: BurnEngine, T: BurnEngine> BurnEngine for CombinedEngine<D, T> {
    fn name(&self) -> &'static str {
        "auto"
    }

    /// Both, because a worker registers one version string and an operator
    /// reading it needs to know what is behind each shape of disc.
    fn version(&self) -> String {
        format!(
            "{} {}; {} {}",
            self.data.name(),
            self.data.version(),
            self.toc.name(),
            self.toc.version()
        )
    }

    fn uses_hardware(&self) -> bool {
        self.data.uses_hardware() || self.toc.uses_hardware()
    }

    /// Whatever the engine for that shape supports. The routing is by mode,
    /// so a mode is supported only if the engine it is routed to says so.
    fn supports_mode(&self, mode: WriteMode) -> bool {
        self.for_mode(mode).supports_mode(mode)
    }

    async fn probe_drive(&self, drive: &DriveRef) -> Result<DriveCapabilities, EngineError> {
        let mut capabilities = self.data.probe_drive(drive).await?;
        // The second view is evidence, not a requirement: a drive the data
        // engine can reach is a drive this worker can use for images, whatever
        // the other engine makes of it.
        match self.toc.probe_drive(drive).await {
            Ok(other) => {
                capabilities.engine_evidence.extend(other.engine_evidence);
                capabilities.supports_buffer_underrun_protection |=
                    other.supports_buffer_underrun_protection;
            }
            Err(error) => {
                capabilities
                    .engine_evidence
                    .insert(self.toc.name().to_owned(), format!("probe failed: {error}"));
            }
        }
        Ok(capabilities)
    }

    async fn inspect_medium(&self, drive: &DriveRef) -> Result<MediumInfo, EngineError> {
        self.data.inspect_medium(drive).await
    }

    async fn preflight(&self, plan: &BurnPlan) -> Result<PreflightReport, EngineError> {
        self.for_mode(plan.mode).preflight(plan).await
    }

    async fn write(
        &self,
        plan: &BurnPlan,
        sink: &dyn EventSink,
    ) -> Result<WriteReport, EngineError> {
        self.for_mode(plan.mode).write(plan, sink).await
    }

    async fn verify(
        &self,
        plan: &BurnPlan,
        sink: &dyn EventSink,
    ) -> Result<VerifyReport, EngineError> {
        self.for_mode(plan.mode).verify(plan, sink).await
    }

    async fn blank(
        &self,
        request: &BlankRequest,
        sink: &dyn EventSink,
    ) -> Result<BlankReport, EngineError> {
        self.data.blank(request, sink).await
    }

    async fn eject(&self, drive: &DriveRef) -> Result<(), EngineError> {
        self.data.eject(drive).await
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::engine::NullSink;
    use std::sync::Mutex;
    use tangible_domain::{BurnAttemptId, DriveId, WorkerId};

    /// An engine that records what it was asked and answers with its name.
    #[derive(Debug)]
    struct Recording {
        name: &'static str,
        modes: Vec<WriteMode>,
        calls: Mutex<Vec<&'static str>>,
        probe_fails: bool,
    }

    impl Recording {
        fn new(name: &'static str, modes: Vec<WriteMode>) -> Self {
            Self {
                name,
                modes,
                calls: Mutex::new(Vec::new()),
                probe_fails: false,
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().expect("lock").clone()
        }

        fn record(&self, call: &'static str) {
            self.calls.lock().expect("lock").push(call);
        }
    }

    #[async_trait]
    impl BurnEngine for Recording {
        fn name(&self) -> &'static str {
            self.name
        }
        fn version(&self) -> String {
            "1.0".to_owned()
        }
        fn uses_hardware(&self) -> bool {
            true
        }
        fn supports_mode(&self, mode: WriteMode) -> bool {
            self.modes.contains(&mode)
        }
        async fn probe_drive(&self, _drive: &DriveRef) -> Result<DriveCapabilities, EngineError> {
            self.record("probe");
            if self.probe_fails {
                return Err(EngineError::DriveUnavailable {
                    alias: "x".to_owned(),
                });
            }
            let mut capabilities = DriveCapabilities::default();
            capabilities
                .engine_evidence
                .insert(self.name.to_owned(), "seen".to_owned());
            Ok(capabilities)
        }
        async fn inspect_medium(&self, _drive: &DriveRef) -> Result<MediumInfo, EngineError> {
            self.record("inspect");
            Ok(MediumInfo {
                profile: self.name.to_owned(),
                ..MediumInfo::default()
            })
        }
        async fn preflight(&self, _plan: &BurnPlan) -> Result<PreflightReport, EngineError> {
            self.record("preflight");
            Ok(PreflightReport {
                failures: Vec::new(),
                warnings: Vec::new(),
                medium: None,
            })
        }
        async fn write(
            &self,
            _plan: &BurnPlan,
            _sink: &dyn EventSink,
        ) -> Result<WriteReport, EngineError> {
            self.record("write");
            Ok(WriteReport {
                engine_reported_success: true,
                bytes_written: 0,
                engine: self.name.to_owned(),
                engine_version: "1.0".to_owned(),
                finalized: true,
                diagnostics: Vec::new(),
            })
        }
        async fn verify(
            &self,
            _plan: &BurnPlan,
            _sink: &dyn EventSink,
        ) -> Result<VerifyReport, EngineError> {
            self.record("verify");
            Err(EngineError::Unsupported {
                what: self.name.to_owned(),
            })
        }
        async fn blank(
            &self,
            _request: &BlankRequest,
            _sink: &dyn EventSink,
        ) -> Result<BlankReport, EngineError> {
            self.record("blank");
            Err(EngineError::DestructiveNotConfirmed)
        }
        async fn eject(&self, _drive: &DriveRef) -> Result<(), EngineError> {
            self.record("eject");
            Ok(())
        }
    }

    fn engine() -> CombinedEngine<Recording, Recording> {
        CombinedEngine::new(
            Recording::new(
                "data",
                vec![WriteMode::DataDiscAtOnce, WriteMode::DataTrackAtOnce],
            ),
            Recording::new("toc", vec![WriteMode::TocDiscAtOnce]),
        )
    }

    fn drive() -> DriveRef {
        DriveRef {
            worker_id: WorkerId::generate(),
            drive_id: DriveId::generate(),
            device_alias: "/dev/disc-block".to_owned(),
        }
    }

    fn plan(mode: WriteMode) -> BurnPlan {
        BurnPlan {
            attempt_id: BurnAttemptId::generate(),
            drive: drive(),
            inputs: Vec::new(),
            tracks: Vec::new(),
            catalog: None,
            mode,
            accepted_profiles: Vec::new(),
            speed: None,
            finalize: true,
            eject_on_success: true,
            total_bytes: 0,
        }
    }

    #[tokio::test]
    async fn an_image_goes_to_the_data_engine_and_a_layout_to_the_other() {
        let engine = engine();
        for mode in [WriteMode::DataDiscAtOnce, WriteMode::DataTrackAtOnce] {
            let plan = plan(mode);
            engine.preflight(&plan).await.expect("preflight");
            let report = engine.write(&plan, &NullSink).await.expect("write");
            assert_eq!(report.engine, "data", "the engine that wrote names itself");
        }
        let layout = plan(WriteMode::TocDiscAtOnce);
        engine.preflight(&layout).await.expect("preflight");
        let report = engine.write(&layout, &NullSink).await.expect("write");
        assert_eq!(report.engine, "toc");
        let _ = engine.verify(&layout, &NullSink).await;

        assert_eq!(
            engine.data.calls(),
            vec!["preflight", "write", "preflight", "write"]
        );
        assert_eq!(engine.toc.calls(), vec!["preflight", "write", "verify"]);
    }

    #[tokio::test]
    async fn a_layout_is_never_verified_by_the_image_engine() {
        // The image engine's read-back of a disc of tracks would be a byte
        // comparison of the wrong thing. The layout engine says it cannot
        // verify, and that answer is the one that reaches the runner.
        let engine = engine();
        let error = engine
            .verify(&plan(WriteMode::TocDiscAtOnce), &NullSink)
            .await
            .expect_err("the layout engine does not verify");
        assert!(matches!(error, EngineError::Unsupported { what } if what == "toc"));
        assert!(engine.data.calls().is_empty());
    }

    #[test]
    fn every_mode_is_supported_by_the_engine_it_goes_to() {
        let engine = engine();
        assert!(engine.supports_mode(WriteMode::DataDiscAtOnce));
        assert!(engine.supports_mode(WriteMode::DataTrackAtOnce));
        assert!(engine.supports_mode(WriteMode::TocDiscAtOnce));

        // And only by that one: routing is by mode, so a data engine that
        // claimed tables of contents would not make them supported here.
        let lopsided = CombinedEngine::new(
            Recording::new("data", vec![WriteMode::DataDiscAtOnce]),
            Recording::new("toc", vec![]),
        );
        assert!(!lopsided.supports_mode(WriteMode::TocDiscAtOnce));
        assert!(!lopsided.supports_mode(WriteMode::DataTrackAtOnce));
    }

    #[tokio::test]
    async fn the_drive_is_asked_about_by_the_data_engine_and_probed_by_both() {
        let engine = engine();
        let drive = drive();
        assert_eq!(
            engine
                .inspect_medium(&drive)
                .await
                .expect("inspect")
                .profile,
            "data"
        );
        engine.eject(&drive).await.expect("eject");
        let capabilities = engine.probe_drive(&drive).await.expect("probe");

        assert!(capabilities.engine_evidence.contains_key("data"));
        assert!(capabilities.engine_evidence.contains_key("toc"));
        assert_eq!(engine.data.calls(), vec!["inspect", "eject", "probe"]);
        assert_eq!(engine.toc.calls(), vec!["probe"]);
    }

    #[tokio::test]
    async fn a_failed_second_probe_is_recorded_rather_than_fatal() {
        let mut toc = Recording::new("toc", vec![WriteMode::TocDiscAtOnce]);
        toc.probe_fails = true;
        let engine = CombinedEngine::new(Recording::new("data", vec![]), toc);

        let capabilities = engine.probe_drive(&drive()).await.expect("probe");
        assert!(
            capabilities.engine_evidence["toc"].starts_with("probe failed"),
            "{:?}",
            capabilities.engine_evidence
        );
    }

    #[test]
    fn the_wrapper_names_both_engines_in_its_version() {
        assert_eq!(engine().name(), "auto");
        assert_eq!(engine().version(), "data 1.0; toc 1.0");
    }
}

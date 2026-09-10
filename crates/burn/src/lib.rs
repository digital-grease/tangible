// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Burn plans, engine adapters, drive capabilities, and verification.
//!
//! Engines are invoked with fixed argument arrays and never through a shell.
//! A successful tool exit is not verified media, so write and verify stay
//! separate operations.
//!
//! In place: the plan and report types, the `BurnEngine` contract, and a
//! hardware-free engine backed by a file standing in for the disc. Real
//! engine adapters follow.

pub mod cdrdao;
pub mod client;
pub mod device_lock;
pub mod engine;
pub mod fake;
pub mod plan;
pub mod runner;
pub mod worker;
pub mod xorriso;

pub use cdrdao::{TocError, write_toc};
pub use client::{
    Capabilities, ClientError, Completion, CompletionAck, DriveDescription, EngineDescription,
    Enrolled, FailureBody, Leased, PhysicalMediumBody, VerificationReportBody, WorkerClient,
    WorkerIdentity, WriteReportBody,
};
pub use device_lock::{DeviceLock, DeviceLockError};
pub use engine::{BurnEngine, BurnEvent, CollectingSink, EngineError, EventSink, NullSink};
pub use fake::{CancelToken, FakeBehaviour, FakeEngine};
pub use plan::{
    BlankReport, BlankRequest, BurnPlan, DriveCapabilities, DriveRef, MediumInfo, PlannedInput,
    PreflightFailure, PreflightReport, VerifyReport, WriteMode, WriteReport,
};
pub use runner::{RunnerError, WorkerRuntime, WorkerSettings};
pub use worker::{
    EventBuffer, EventError, Lease, RecoveryDirective, RecoveryError, RecoveryPlan, RecoveryRecord,
    RecoveryStore, UnknownStage, WorkerEvent, WorkerStage, plan_for,
};

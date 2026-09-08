// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The xorriso adapter.
//!
//! Two halves, kept apart on purpose. [`command`] turns a validated plan into
//! a fixed argument array and can add nothing that was not already in the
//! plan; [`parse`] reads what the tool said and decides nothing.
//!
//! Everything here is pure. Running the process is the engine's job, which
//! means the dangerous part (what arguments reach a program that can destroy
//! a disc) is testable without a drive, and the fragile part (what the tool
//! prints) is tested against output captured from the real thing.

pub mod command;
pub mod engine;
pub mod parse;

pub use command::XORRISO;
pub use engine::XorrisoEngine;
pub use parse::{Device, MediumReport, Version, WriteEvent, WriteOutcome};

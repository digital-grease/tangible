// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The cdrdao adapter: disc-at-once CD writing from a table of contents.
//!
//! Four parts, kept apart for the reason the xorriso adapter's are. [`toc`]
//! turns a validated plan into the document cdrdao reads, [`command`] into the
//! fixed arguments it runs with, and neither can add anything the plan did not
//! already say. [`parse`] reads what the tool printed and decides nothing.
//! [`engine`] runs the process and is the only part that touches a drive.

pub mod command;
pub mod engine;
pub mod parse;
pub mod toc;

pub use command::{CDRDAO, CommandError};
pub use engine::CdrdaoEngine;
pub use toc::{TocError, write_toc};

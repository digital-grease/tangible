// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The cdrdao adapter: disc-at-once CD writing from a table of contents.
//!
//! In place: the TOC writer, which turns a validated [`crate::plan::BurnPlan`]
//! into the document cdrdao reads. The engine that runs the tool follows.

pub mod toc;

pub use toc::{TocError, write_toc};

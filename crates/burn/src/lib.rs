// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Burn plans, engine adapters, drive capabilities, and verification.
//!
//! Engines are invoked with fixed argument arrays and never through a shell.
//! A successful tool exit is not verified media, so write and verify stay
//! separate operations.
//!
//! Scaffold status: empty. The `BurnEngine` trait and fake engine arrive with
//! epic E5; the xorriso adapter with epic E6.

// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Disc-image format detection, parsing, and structural validation.
//!
//! Every input here is hostile. Detection uses signatures and structure
//! rather than filenames, parsers read bytes and never execute them, and no
//! image is ever mounted.
//!
//! Scaffold status: empty. ISO detection arrives with epic E4, CUE/BIN with
//! epic E7.

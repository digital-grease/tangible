// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Rebuild when a migration is added or changed.
//!
//! `sqlx::migrate!` embeds `migrations/` when this crate compiles, and cargo
//! does not otherwise know to look there: a new migration file would be left
//! out of the binary until something else in the crate changed.

fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}

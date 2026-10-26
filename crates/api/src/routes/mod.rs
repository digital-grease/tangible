// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP routes, grouped by the resource they serve.

pub mod accounts;
pub mod artifacts;
pub mod burns;
pub mod catalog;
pub mod derivations;
pub mod diagnostics;
pub mod erasures;
pub mod imports;
pub mod integration_jobs;
pub mod integrations;
pub mod integrity;
pub mod physical_copies;
pub mod reports;
pub mod rips;
pub mod romm;
pub mod s3_imports;
pub mod uploads;
pub mod workers;

// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Outbound integrations: RomM export, download clients, metadata providers,
//! and Prowlarr.
//!
//! Integrations submit a job a person chose to a client the operator
//! already runs. For an approved request (ADR 0021), Prowlarr is searched
//! and a person picks the release; nothing is ever searched for or chosen
//! on its own.
//!
//! In place: exporting games to a RomM library as a managed folder view,
//! RomM's API for what it made of them, the download clients, S3, the
//! metadata providers a request is found through, Prowlarr, and the
//! outbound client every fetch of an address somebody supplied goes through
//! (ADR 0016).

pub mod download;
pub mod fetch;
pub mod metadata;
pub mod nzb;
pub mod nzbget;
pub mod oidc;
pub mod outbound;
pub mod prowlarr;
pub mod qbittorrent;
pub mod romm;
pub mod romm_api;
pub mod s3;
pub mod sabnzbd;
pub mod settings;
pub mod torrent;

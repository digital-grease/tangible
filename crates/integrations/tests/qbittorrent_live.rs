// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The qBittorrent client against a real qBittorrent, when one is named.
//!
//! Gated like the hardware tests: each skips unless
//! `TANGIBLE_TEST_QBITTORRENT_URL` names a throwaway client (such as
//! `http://127.0.0.1:18080/`), with `TANGIBLE_TEST_QBITTORRENT_USER`
//! (default `admin`) and `TANGIBLE_TEST_QBITTORRENT_PASSWORD`. The WebUI's
//! port must be the one it is reached on, or its host header check refuses:
//!
//! ```sh
//! docker run -d --name qbt -e QBT_LEGAL_NOTICE=confirm -e QBT_WEBUI_PORT=18080 \
//!   -p 127.0.0.1:18080:18080 qbittorrentofficial/qbittorrent-nox:5.1.2-1
//! docker logs qbt   # the temporary password
//! ```
//!
//! It adds the private, trackerless fixture torrent to the category
//! `tangible-test`, which reaches no peer, and leaves it there: Tangible
//! never removes a torrent.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_domain::SecretString;
use tangible_integrations::download::{ClientError, Phase};
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy, Url};
use tangible_integrations::qbittorrent::{Added, QbittorrentClient, TorrentSource};
use tangible_integrations::torrent::infohash_of_torrent;

fn client(password: Option<&str>) -> Option<QbittorrentClient> {
    let Ok(url) = std::env::var("TANGIBLE_TEST_QBITTORRENT_URL") else {
        eprintln!("skipping: set TANGIBLE_TEST_QBITTORRENT_URL to test against a qBittorrent");
        return None;
    };
    let user = std::env::var("TANGIBLE_TEST_QBITTORRENT_USER").unwrap_or_else(|_| "admin".into());
    let password = password
        .map(str::to_owned)
        .or_else(|| std::env::var("TANGIBLE_TEST_QBITTORRENT_PASSWORD").ok());
    let policy = OutboundPolicy::for_integrations(vec!["127.0.0.0/8".parse().unwrap()], None);
    let http = OutboundClient::new(policy, "tangible-test").unwrap();
    Some(QbittorrentClient::new(
        http,
        Url::parse(&url).unwrap(),
        &user,
        password.map(SecretString::from),
    ))
}

#[tokio::test]
async fn a_real_qbittorrent_signs_in_adds_and_reports_by_hash() {
    let Some(mut client) = client(None) else {
        return;
    };
    let version = client.api_version().await.unwrap();
    assert!(version.starts_with("2."), "{version}");

    let torrent = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/tool-output/qbittorrent/single-file.torrent"
    ))
    .unwrap();
    let hash = infohash_of_torrent(&torrent).unwrap();
    let first = client
        .add(&TorrentSource::File(&torrent), "tangible-test")
        .await
        .unwrap();
    // Added now, or declined because an earlier run added it.
    assert!(matches!(first, Added::Added | Added::Declined));
    let again = client
        .add(&TorrentSource::File(&torrent), "tangible-test")
        .await
        .unwrap();
    assert_eq!(again, Added::Declined, "a torrent it has is declined");

    let mut status = None;
    for _ in 0..20 {
        status = client.torrent(&hash).await.unwrap();
        if status.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let status = status.expect("the client has the torrent");
    assert_eq!(status.hash, hash.as_str());
    assert!(matches!(
        status.phase(),
        Phase::Downloading { .. } | Phase::Complete { .. }
    ));
    assert!(
        client
            .torrent(&infohash_of_torrent(b"d4:infod6:pieces0:ee").unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_wrong_password_is_told_apart() {
    let Some(mut client) = client(Some("certainly-not-the-password")) else {
        return;
    };
    let error = client.sign_in().await.expect_err("refused");
    assert!(matches!(error, ClientError::Auth { .. }), "{error}");
    assert_eq!(error.code(), "INTEGRATION_AUTH_FAILED");
    assert!(error.to_string().contains("wrong"), "{error}");
}

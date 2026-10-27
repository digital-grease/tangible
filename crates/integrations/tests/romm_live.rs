// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The RomM client against a real RomM, when one is named.
//!
//! Gated like the hardware tests: each skips unless `TANGIBLE_TEST_ROMM_URL`
//! names a throwaway RomM (such as `http://127.0.0.1:18080/`) and
//! `TANGIBLE_TEST_ROMM_TOKEN` a client token made in it with the scopes
//! `platforms.read`, `roms.read` and `tasks.run`. RomM needs a database; the
//! Compose project in fixtures/tool-output/romm/README.md runs both.
//!
//! The only thing asked of RomM that is not a read is a quick scan of a
//! platform folder no library has, which scans nothing.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use tangible_domain::SecretString;
use tangible_integrations::download::ClientError;
use tangible_integrations::outbound::{OutboundClient, OutboundPolicy, Url};
use tangible_integrations::romm_api::{RommClient, Scope, TaskPhase};

fn client(token: Option<&str>) -> Option<RommClient> {
    let Ok(url) = std::env::var("TANGIBLE_TEST_ROMM_URL") else {
        eprintln!("skipping: set TANGIBLE_TEST_ROMM_URL to test against a RomM");
        return None;
    };
    let token = token
        .map(str::to_owned)
        .or_else(|| std::env::var("TANGIBLE_TEST_ROMM_TOKEN").ok())
        .expect("TANGIBLE_TEST_ROMM_TOKEN names a client token");
    let policy = OutboundPolicy::for_integrations(vec!["127.0.0.0/8".parse().unwrap()], None);
    let http = OutboundClient::new(policy, "tangible-test").unwrap();
    Some(RommClient::new(
        http,
        Url::parse(&url).unwrap(),
        SecretString::new(token),
    ))
}

#[tokio::test]
async fn a_scoped_token_finds_everything_tangible_needs() {
    let Some(client) = client(None) else { return };
    let probe = client.probe().await.unwrap();
    assert!(!probe.version.is_empty());
    for scope in Scope::ALL {
        assert!(probe.granted(scope), "{scope:?}");
    }
    assert!(probe.can_link() && probe.can_scan(), "{probe:?}");
    assert!(
        client
            .known_platforms()
            .await
            .unwrap()
            .iter()
            .any(|slug| slug == "psx")
    );
    for platform in client.platforms().await.unwrap() {
        let page = client.roms(platform.id, None, 0, 5).await.unwrap();
        for rom in page.items {
            assert_eq!(rom.platform_id, platform.id);
            assert_eq!(client.rom(rom.id).await.unwrap().unwrap().id, rom.id);
        }
    }
    assert_eq!(client.rom(i64::from(i32::MAX)).await.unwrap(), None);
}

#[tokio::test]
async fn a_scan_is_queued_and_followed_to_its_end() {
    let Some(client) = client(None) else { return };
    let queued = client
        .scan(&["tangible-test-nothing".to_owned()])
        .await
        .unwrap();
    let mut phase = queued.phase;
    for _ in 0..60 {
        let task = client.task(&queued.id).await.unwrap().expect("RomM has it");
        phase = task.phase;
        if phase.ended() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    assert_eq!(phase, TaskPhase::Finished);
    assert_eq!(
        client
            .task("00000000-0000-0000-0000-000000000000")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn a_wrong_token_is_refused_and_not_repeated() {
    let Some(client) = client(Some("rmm_not-a-real-token")) else {
        return;
    };
    // The version needs no token.
    assert!(client.version().await.is_ok());
    let error = client.platforms().await.unwrap_err();
    assert!(matches!(error, ClientError::Auth { .. }), "{error}");
    assert!(!error.to_string().contains("not-a-real"), "{error}");
}

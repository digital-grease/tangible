// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A worker refuses to run without its drive.
//!
//! The claim is taken before enrollment, so these need no server: a worker
//! that gets past the drive is one that goes on to fail for the ordinary
//! reason it has no credential, and that difference is the assertion.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;

use tangible_burn::device_lock::{DeviceLock, DeviceLockError};
use tangible_burn::runner::{RunnerError, WorkerRuntime, WorkerSettings};
use tangible_burn::xorriso::XorrisoEngine;
use tempfile::TempDir;

/// A file standing in for a device node, and settings that name it.
fn worker(dir: &TempDir) -> (PathBuf, WorkerSettings) {
    let node = dir.path().join("disc-block");
    std::fs::write(&node, b"").expect("create the node");

    let settings = WorkerSettings {
        device_alias: node.to_string_lossy().into_owned(),
        // Never reached. The drive is claimed before anything is sent.
        ..WorkerSettings::new("http://127.0.0.1:1", "worker", dir.path().join("state"))
    };
    (node, settings)
}

#[tokio::test]
async fn a_worker_whose_drive_is_taken_refuses_to_start() {
    let dir = TempDir::new().expect("tempdir");
    let (node, settings) = worker(&dir);
    let held = DeviceLock::acquire(&node).expect("claim the drive first");

    let mut runtime = WorkerRuntime::new(settings, XorrisoEngine::new()).expect("runtime");
    let error = runtime
        .run(std::future::pending())
        .await
        .expect_err("a worker without its drive must not run");

    assert!(
        matches!(
            error,
            RunnerError::Drive(DeviceLockError::AlreadyHeld { .. })
        ),
        "expected a refusal over the drive, got {error:?}"
    );
    drop(held);
}

#[tokio::test]
async fn a_worker_with_its_drive_gets_past_the_claim() {
    // The control for the test above: without it, a worker that refused to
    // start for any reason at all would look like the drive check working.
    let dir = TempDir::new().expect("tempdir");
    let (_node, settings) = worker(&dir);

    let mut runtime = WorkerRuntime::new(settings, XorrisoEngine::new()).expect("runtime");
    let error = runtime
        .run(std::future::pending())
        .await
        .expect_err("there is no credential and no token");

    assert!(
        matches!(error, RunnerError::NotEnrolled),
        "expected to reach enrollment, got {error:?}"
    );
}

#[tokio::test]
async fn a_worker_releases_its_drive_when_it_stops() {
    // A redeploy is a worker stopping and another starting on the same drive.
    // The second must not have to wait for anything to time out.
    let dir = TempDir::new().expect("tempdir");
    let (node, settings) = worker(&dir);

    let mut runtime = WorkerRuntime::new(settings, XorrisoEngine::new()).expect("runtime");
    let _ = runtime.run(std::future::pending()).await;
    drop(runtime);

    DeviceLock::acquire(&node).expect("the drive is claimable again");
}

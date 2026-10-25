// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The loop the server's queue runners share.
//!
//! A step is never cut short by shutdown: it finishes, and shutdown is looked
//! at between steps and while idle. Racing a step against shutdown dropped it
//! mid-way, and a step dropped in the middle of a database statement left its
//! connection holding an open transaction and a row lock nobody would ever
//! release; the next write to that row waited forever. Found in the worker
//! integration tests, 2026-10-25, as imports that never settled.

use std::future::Future;

use futures_util::FutureExt as _;

/// Run `step` until `shutdown` resolves. `step` answers whether it found work;
/// when it did not, `idle` is awaited before the next, unless shutdown comes
/// first. A step that has started always finishes.
pub async fn run_steps<S, Step, StepFuture, Idle, IdleFuture>(
    shutdown: S,
    mut step: Step,
    mut idle: Idle,
) where
    S: Future<Output = ()>,
    Step: FnMut() -> StepFuture,
    StepFuture: Future<Output = bool>,
    Idle: FnMut() -> IdleFuture,
    IdleFuture: Future<Output = ()>,
{
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        if (&mut shutdown).now_or_never().is_some() {
            return;
        }
        if step().await {
            continue;
        }
        tokio::select! {
            () = &mut shutdown => return,
            () = idle() => {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn a_step_under_way_finishes_although_shutdown_came_during_it() {
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let mut stop = Some(stop);
        let (s, f) = (Arc::clone(&started), Arc::clone(&finished));
        run_steps(
            async {
                let _ = stopped.await;
            },
            move || {
                let (s, f) = (Arc::clone(&s), Arc::clone(&f));
                // Shutdown is asked for as the first step begins.
                let stop = stop.take();
                async move {
                    s.fetch_add(1, Ordering::SeqCst);
                    if let Some(stop) = stop {
                        let _ = stop.send(());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    f.fetch_add(1, Ordering::SeqCst);
                    true
                }
            },
            std::future::pending,
        )
        .await;
        assert_eq!(started.load(Ordering::SeqCst), 1, "no step after shutdown");
        assert_eq!(
            finished.load(Ordering::SeqCst),
            1,
            "the step under way finished"
        );
    }

    #[tokio::test]
    async fn an_idle_runner_stops_at_once() {
        let steps = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&steps);
        let shutdown = tokio::time::sleep(Duration::from_millis(50));
        tokio::time::timeout(
            Duration::from_secs(5),
            run_steps(
                shutdown,
                move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    async { false }
                },
                || tokio::time::sleep(Duration::from_secs(3600)),
            ),
        )
        .await
        .expect("stopped while idle, not after the idle wait");
        assert_eq!(steps.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn work_runs_back_to_back_and_idle_waits_between() {
        let steps = Arc::new(AtomicUsize::new(0));
        let idles = Arc::new(AtomicUsize::new(0));
        let (counted, waited) = (Arc::clone(&steps), Arc::clone(&idles));
        let shutdown = tokio::time::sleep(Duration::from_millis(200));
        run_steps(
            shutdown,
            move || {
                let n = counted.fetch_add(1, Ordering::SeqCst);
                async move { n < 3 }
            },
            move || {
                waited.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10))
            },
        )
        .await;
        assert!(steps.load(Ordering::SeqCst) >= 4);
        assert!(idles.load(Ordering::SeqCst) >= 1);
    }
}

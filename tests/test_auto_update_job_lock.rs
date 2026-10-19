//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Regression test for ANT-348 (parent: ANT-347, the Control page stuck on
//! "Loading…").
//!
//! `background_auto_update_job` used to hold the WRITE lock on
//! `state.auto_update_job` across the entire `tokio::time::sleep` of the
//! update interval. The only reader of that state is
//! `GET /api/admin/apps/auto_update_status`, so with the default 3600 s
//! interval the endpoint was blocked — and the Control page hung on
//! `Promise.all` — for ~99.5% of the time. This test spawns the real job
//! with a short interval and asserts the read lock is acquirable promptly
//! (i.e. the write guard is gone) by the time the first tick's record is
//! visible and while the job is sleeping.

mod common;

use std::time::{Duration, Instant};

use common::{TempDir, build_state_with};
use vreckan::catalog;

/// Seconds between job ticks. Long enough that the buggy variant (write
/// guard held across the sleep) is unambiguously distinguishable from the
/// fixed one within the polling budgets below.
const INTERVAL_SECS: u64 = 5;

#[tokio::test]
async fn test_auto_update_job_drops_write_lock_before_sleep() {
    let _root = TempDir::new();
    let state = build_state_with(_root.path(), |s| {
        // No-op tick (avoids Docker); the interval drives the sleep length.
        s.auto_update_apps = false;
        s.auto_update_interval_seconds = INTERVAL_SECS as i64;
    })
    .await;
    let job_state = state.auto_update_job.clone();

    let started = Instant::now();
    let handle = tokio::spawn(catalog::background_auto_update_job(state));

    // Wait for the first tick to complete its bookkeeping (the first tick
    // runs immediately at startup). The buggy variant held the write guard
    // across the whole sleep, so no read could observe the record until the
    // full interval elapsed; the fixed variant drops the guard right after
    // the write, so the observation must arrive far sooner than that.
    let deadline = started + Duration::from_secs(10);
    let mut observed = None;
    while Instant::now() < deadline {
        if let Ok(guard) = tokio::time::timeout(Duration::from_millis(200), job_state.read()).await
        {
            if guard.last_check_at.is_some() {
                observed = Some((started.elapsed(), guard.last_check_at, guard.next_check_at));
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (observed_at, last_check_at, next_check_at) =
        observed.expect("first tick never recorded last_check_at");
    assert!(
        observed_at < Duration::from_secs(INTERVAL_SECS / 2),
        "first tick's record was only observable after {observed_at:?} — the write lock was still held (ANT-348)"
    );

    // While the job is sleeping, the read lock must be acquirable promptly —
    // exactly what GET /api/admin/apps/auto_update_status does.
    let t0 = Instant::now();
    let _guard = tokio::time::timeout(Duration::from_secs(1), job_state.read())
        .await
        .expect("read lock not acquired promptly while the auto-update job is sleeping (ANT-348)");
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "read lock took {:?} to acquire",
        t0.elapsed()
    );

    // The record must reflect the completed tick: next = last + interval.
    let last = last_check_at.expect("last_check_at set but None");
    let next = next_check_at.expect("next_check_at set but None");
    assert!(
        (next - last - INTERVAL_SECS as f64).abs() < 1.0,
        "next_check_at ({next}) is not last_check_at ({last}) + interval"
    );

    handle.abort();
}

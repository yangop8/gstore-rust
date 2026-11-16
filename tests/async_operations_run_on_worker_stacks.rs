//! Asynchronous operations run on threads with the HTTP workers' stack size
//! (review design#8): a job that needs more than the 2 MiB a default thread
//! gets — what a deep but allowed query needs in an unoptimized build —
//! completes on a task runner. (Before the fix the runner overflowed its
//! stack and the process aborted.)

use std::time::Duration;

use gstore::gapi::tasks::{TaskConfig, TaskManager, TaskSpec, TaskState};
use gstore::json::Json;

/// Recurse `depth` frames of 64 KiB each.
#[inline(never)]
fn burn(depth: u32) -> u64 {
    let buf = std::hint::black_box([depth as u8; 64 * 1024]);
    if depth == 0 {
        u64::from(buf[0])
    } else {
        burn(depth - 1) + u64::from(std::hint::black_box(buf[buf.len() - 1]))
    }
}

#[test]
fn an_asynchronous_job_gets_the_worker_stack() {
    let manager = TaskManager::new(TaskConfig {
        workers: 1,
        ..TaskConfig::default()
    });
    // About 4 MiB: more than a default thread's 2 MiB, less than the
    // workers' 8 MiB (release) / 16 MiB (debug).
    let id = manager
        .submit(
            TaskSpec {
                operation: "deep".into(),
                ..TaskSpec::default()
            },
            Box::new(|_| Json::obj().with("StatusCode", 0).with("burnt", burn(64))),
        )
        .unwrap();
    assert_eq!(
        manager.wait_finished(id, Duration::from_secs(60)),
        Some(TaskState::Succeeded)
    );
}

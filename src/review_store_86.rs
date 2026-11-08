//! Review-only probes for PR 86. No production behavior is changed.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

#[test]
fn review_host_refused_writes_are_bounded_by_the_overdue_cap() {
    let evaluations = Arc::new(Evaluations::default());
    let writer = Writer::default();
    let active = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(WriteLock::default());
    let mut releases = Vec::new();
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let (release, wait) = std::sync::mpsc::channel();
        releases.push(release);
        let active = Arc::clone(&active);
        let writes = Arc::clone(&writes);
        outcomes.push(run(
            "q",
            true,
            Duration::from_millis(100),
            &evaluations,
            Admission {
                limits: Limits::only_the_cap(1),
                caller: Caller::Host,
            },
            Some(&writer),
            move |deadline| {
                let _guard = writes.lock_for(deadline);
                active.fetch_add(1, Ordering::SeqCst);
                let _ = wait.recv_timeout(Duration::from_secs(10));
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            },
        ));
    }
    let pinned = active.load(Ordering::SeqCst);
    let overdue = evaluations.overdue();
    for release in releases {
        let _ = release.send(());
    }
    let until = Instant::now() + Duration::from_secs(5);
    while evaluations.overdue() != 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(pinned <= 1, "cap=1 but {pinned} separate writer threads are pinned; overdue={overdue}; answers={outcomes:?}");
}

#[test]
fn review_takeover_and_stale_guard_drop_do_not_release_the_new_holder() {
    let writes = Arc::new(WriteLock::default());
    let first = Deadline::never();
    let old = writes.lock_for(&first);
    *lock(&first.state) = State::Abandoned;
    let next = writes.lock();
    drop(old);
    assert!(writes.holding.lock().unwrap().holder.is_some());
    let (tx, rx) = std::sync::mpsc::channel();
    let other = Arc::clone(&writes);
    let waiter = std::thread::spawn(move || {
        let _guard = other.lock();
        tx.send(()).unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    drop(next);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    waiter.join().unwrap();
}

#[test]
fn review_abandonment_and_commit_are_mutually_exclusive() {
    let deadline = Arc::new(Deadline::never());
    let committing = Arc::clone(&deadline);
    let (entered, entered_rx) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let commit = std::thread::spawn(move || {
        committing.settle(|| {
            entered.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        deadline.state.try_lock().is_err(),
        "commit must retain the state mutex"
    );
    release.send(()).unwrap();
    commit.join().unwrap().unwrap();
    assert!(*lock(&deadline.state) == State::Settled);
    *lock(&deadline.state) = State::Abandoned;
    assert!(matches!(
        deadline.settle::<()>(|| panic!("abandoned commit ran")),
        Err(Error::Timeout(_))
    ));
    assert!(*lock(&deadline.state) == State::Abandoned);
}

#[test]
fn review_replacement_preserves_queued_jobs_and_replies() {
    let writer = Writer::default();
    let (started, started_rx) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    writer
        .submit(
            crate::limits::STACK_BASE,
            Box::new(move || {
                started.send(std::thread::current().id()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }),
        )
        .unwrap();
    let stuck = started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut answers = Vec::new();
    let order = Arc::new(Mutex::new(Vec::new()));
    for index in 0..20 {
        let (answer, received) = std::sync::mpsc::channel();
        answers.push(received);
        let order = Arc::clone(&order);
        writer
            .submit(
                crate::limits::STACK_BASE,
                Box::new(move || {
                    order.lock().unwrap().push(index);
                    answer.send(index).unwrap();
                }),
            )
            .unwrap();
    }
    writer.replace(stuck);
    for (index, received) in answers.into_iter().enumerate() {
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            index
        );
    }
    assert_eq!(*order.lock().unwrap(), (0..20).collect::<Vec<_>>());
    release.send(()).unwrap();
}

#[test]
fn review_panicking_guard_releases_the_hold() {
    let writes = WriteLock::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = writes.lock();
        panic!("review guard unwind");
    }));
    assert!(result.is_err());
    assert!(writes.holding.lock().unwrap().holder.is_none());
    let _guard = writes.lock();
}
